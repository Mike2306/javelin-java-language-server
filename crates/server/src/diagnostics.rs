//! Unresolved-type, unresolved-method and unresolved-field diagnostics: a
//! type name in a file that resolves to nothing; a method called, or a field
//! accessed, on a receiver whose type is certain, whose member list is whole,
//! and which has no member of that name; and an unqualified call no type
//! around it can mean.
//!
//! The constraint is the design — a false positive costs more than ten
//! missing answers, because a reviewer shown one wrong squiggle stops reading
//! all of them. So everything here errs towards silence, and a file that fails
//! a file-level gate gets nothing at all: a partial set is indistinguishable
//! from a whole one to the person reading it. Whether the *world* is whole —
//! the JDK in, the workspace walked, the dependencies resolved — is the
//! caller's gate; this module judges one file against a world it is told is
//! whole.
//!
//! ponytail: of members, a method reference and a bare field name are not
//! diagnosed, nor a field through a type name this file declares, whose
//! inherited nested types no list holds. A qualified type name is diagnosed only when it is qualified by a package
//! — `com.example.Missing` — and never when any part of it is a type,
//! because `HashMap.Entry` is `Map.Entry` inherited and no fact lists a
//! type's inherited member types. The upgrades are the parser naming what a
//! field access's left side is, and member types in the closure.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;

use javelin_classpath::{DeclKind, LombokUse, Resolution, TypeWorld};
use javelin_core::{CancellationToken, Cancelled};
use javelin_document::{DocumentId, Range, TextDocument};
use javelin_parser::{Decl, FileSyntax, JavaParser, ScopeTree, Span};

use crate::classpath::ClasspathExternals;

/// Names the grammar can read as a type that are keywords where they appear.
const CONTEXTUAL: [&str; 6] = ["var", "record", "yield", "sealed", "permits", "when"];

/// Everything besides the file a judgement reads.
pub(crate) struct Context<'a> {
    pub externals: &'a ClasspathExternals,
    /// What each module compiles against: a file is judged against its own
    /// module's JARs, and not at all where no module claims it.
    pub modules: Option<&'a std::sync::Arc<crate::modules::Modules>>,
    /// The simple names of the types a build has generated where the walk
    /// does not look.
    pub generated: &'a BTreeSet<String>,
    /// A dependency registers an annotation processor.
    pub processors: bool,
    /// The folders the client named, where the build that was asked lives.
    pub roots: &'a [std::path::PathBuf],
    /// Directories whose sources compile against a classpath nobody asked
    /// for — `javelin_index::Survey::unasked`.
    pub unasked: &'a BTreeSet<std::path::PathBuf>,
    /// The lowest Java level the project may compile at: what the JDK
    /// removed since is exempt. `u32::MAX` for a project this JDK compiles.
    pub level: u32,
    /// The feature version of the JDK the world was read from.
    pub jdk: u32,
    /// Calls are judged as well as type names — see `Snapshot::members`.
    pub members: bool,
}

/// What the JDK removed after Java 8, and the release it went in — a type,
/// or a package when it names one. The world is the JDK's, so a project
/// compiling at an earlier level can name them and the world cannot.
const REMOVED: &[(&str, u32)] = &[
    ("javax.xml.bind", 11),
    ("javax.activation", 11),
    ("javax.xml.ws", 11),
    ("javax.jws", 11),
    ("javax.xml.soap", 11),
    ("javax.annotation.PostConstruct", 11),
    ("javax.annotation.PreDestroy", 11),
    ("javax.annotation.Resource", 11),
    ("javax.annotation.Resources", 11),
    ("javax.annotation.Generated", 11),
    ("javax.annotation.ManagedBean", 11),
    ("javax.transaction.InvalidTransactionException", 11),
    ("javax.transaction.TransactionRequiredException", 11),
    ("javax.transaction.TransactionRolledbackException", 11),
    ("javax.activity", 11),
    ("javax.rmi.CORBA", 11),
    ("org.omg", 11),
    ("javafx", 11),
    ("java.util.jar.Pack200", 14),
    ("java.security.acl", 14),
    ("jdk.nashorn", 15),
    ("java.rmi.activation", 17),
    ("javax.security.auth.Policy", 17),
    ("java.lang.Compiler", 21),
    ("java.applet", 26),
    ("javax.swing.JApplet", 26),
    ("java.beans.AppletInitializer", 26),
];

/// Whether `fqn` — or, `qualified` false, a simple name — is something the
/// JDK removed after the project's level.
fn removed(name: &str, qualified: bool, level: u32) -> bool {
    REMOVED.iter().any(|(removed, since)| {
        level < *since
            && if qualified {
                name == *removed || name.starts_with(&format!("{removed}."))
            } else {
                removed.rsplit('.').next() == Some(name)
            }
    })
}

/// One name that resolves to nothing, where it is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unresolved {
    pub name: String,
    pub range: Range,
    pub kind: Kind,
}

/// What an unresolved name was meant to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kind {
    Type,
    /// A method called on a receiver of this type.
    Method {
        receiver: String,
    },
    /// A field accessed on a receiver of this type.
    Field {
        receiver: String,
    },
    /// A method called with no receiver.
    Unqualified,
}

/// A world a caller has found whole, as the worker judges against it.
pub(crate) struct Snapshot {
    pub externals: ClasspathExternals,
    /// What each module compiles against; `None` for one classpath.
    pub modules: Option<std::sync::Arc<crate::modules::Modules>>,
    pub survey: std::sync::Arc<javelin_index::Survey>,
    pub processors: bool,
    /// Calls may be judged too: no tool adds members to declared types, and
    /// the index is level with every buffer.
    pub members: bool,
    pub roots: Vec<std::path::PathBuf>,
    pub level: u32,
    pub jdk: u32,
}

/// What the loop hands the worker.
pub(crate) enum Job {
    /// Judge this document, at this version, against this world.
    Judge {
        uri: String,
        path: std::path::PathBuf,
        document: std::sync::Arc<TextDocument>,
        snapshot: std::sync::Arc<Snapshot>,
    },
    /// The document closed: its diagnostics go with it.
    Clear { uri: String },
}

/// What the worker hands back: the snapshot judged, or `None` for a clear.
#[derive(Debug)]
pub(crate) struct Published {
    pub uri: String,
    pub document: Option<std::sync::Arc<TextDocument>>,
    pub found: Vec<Unresolved>,
}

/// The worker, on a thread of its own, or `None` if it could not start.
///
/// Jobs are folded per document until `debounce` passes with nothing new, or
/// four times that since the first — a typist produces versions faster than
/// a file can be walked, and only the last one is worth judging. Never
/// joined: it ends when the loop's sender is dropped with the state.
pub(crate) fn spawn(
    debounce: std::time::Duration,
    results: crossbeam_channel::Sender<Published>,
) -> Option<crossbeam_channel::Sender<Job>> {
    let (jobs, queued) = crossbeam_channel::unbounded::<Job>();
    std::thread::Builder::new()
        .name("javelin-diagnostics".to_owned())
        .spawn(move || {
            let Ok(mut parser) = JavaParser::new() else {
                return;
            };
            while let Ok(first) = queued.recv() {
                let started = std::time::Instant::now();
                let mut batch: Vec<Job> = vec![first];
                loop {
                    let left = (debounce * 4).saturating_sub(started.elapsed());
                    match queued.recv_timeout(debounce.min(left)) {
                        Ok(job) => {
                            let uri = match &job {
                                Job::Judge { uri, .. } | Job::Clear { uri } => uri.clone(),
                            };
                            batch.retain(|queued| match queued {
                                Job::Judge { uri: other, .. } | Job::Clear { uri: other } => {
                                    *other != uri
                                }
                            });
                            batch.push(job);
                        }
                        Err(_) => break,
                    }
                }
                for job in batch {
                    let published = match job {
                        Job::Clear { uri } => Published {
                            uri,
                            document: None,
                            found: Vec::new(),
                        },
                        Job::Judge {
                            uri,
                            path,
                            document,
                            snapshot,
                        } => {
                            let context = Context {
                                externals: &snapshot.externals,
                                modules: snapshot.modules.as_ref(),
                                generated: &snapshot.survey.generated,
                                processors: snapshot.processors,
                                roots: &snapshot.roots,
                                unasked: &snapshot.survey.unasked,
                                level: snapshot.level,
                                jdk: snapshot.jdk,
                                members: snapshot.members,
                            };
                            let found = unresolved(
                                &path,
                                document.text(),
                                &mut parser,
                                &context,
                                &CancellationToken::new(),
                            )
                            .unwrap_or_default();
                            Published {
                                uri,
                                document: Some(document),
                                found,
                            }
                        }
                    };
                    if results.send(published).is_err() {
                        return;
                    }
                }
            }
        })
        .inspect_err(|err| tracing::warn!(%err, "diagnostics: could not start the worker"))
        .ok()?;
    Some(jobs)
}

/// The notification a published judgement becomes.
///
/// A warning rather than an error: version 1 says a name is unknown, never
/// that a program is wrong.
pub(crate) fn notification(published: &Published) -> lsp_server::Notification {
    let diagnostics: Vec<lsp_types::Diagnostic> = published
        .found
        .iter()
        .map(|found| {
            let (code, message) = match &found.kind {
                Kind::Type => (
                    "unresolved-type",
                    format!(
                        "`{}` is not a type the JDK, the dependencies or the workspace declare",
                        found.name
                    ),
                ),
                // "That can be called here": the lists this reads leave out
                // another top-level type's `private` members, so the claim
                // is true of a private one too.
                Kind::Method { receiver } => (
                    "unresolved-method",
                    format!(
                        "`{receiver}` has no method `{}` that can be called here",
                        found.name
                    ),
                ),
                Kind::Field { receiver } => (
                    "unresolved-field",
                    format!(
                        "`{receiver}` has no field `{}` that can be accessed here",
                        found.name
                    ),
                ),
                Kind::Unqualified => (
                    "unresolved-method",
                    format!(
                        "no method `{}` that can be called here is in scope",
                        found.name
                    ),
                ),
            };
            lsp_types::Diagnostic {
                range: crate::handlers::to_lsp_range(found.range),
                severity: Some(lsp_types::DiagnosticSeverity::WARNING),
                code: Some(lsp_types::NumberOrString::String(code.to_owned())),
                source: Some("javelin".to_owned()),
                message,
                ..lsp_types::Diagnostic::default()
            }
        })
        .collect();
    lsp_server::Notification::new(
        "textDocument/publishDiagnostics".to_owned(),
        serde_json::json!({
            "uri": published.uri,
            "version": published.document.as_ref().map(|document| document.version()),
            "diagnostics": diagnostics,
        }),
    )
}

/// Every type name in `text` — the content of `path` — that certainly
/// resolves to nothing, and every member use naming what its receiver, or
/// every type around it, certainly lacks; or none at all where the file
/// cannot be judged.
pub(crate) fn unresolved(
    path: &Path,
    text: &str,
    parser: &mut JavaParser,
    context: &Context<'_>,
    token: &CancellationToken,
) -> Result<Vec<Unresolved>, Cancelled> {
    token.check()?;
    if in_unasked_build(path, context.roots)
        || context.unasked.iter().any(|dir| path.starts_with(dir))
    {
        return Ok(Vec::new());
    }
    let scoped;
    let narrowed;
    let context = match context.modules.map(|modules| modules.scope_of(path)) {
        None => context,
        Some(None) => return Ok(Vec::new()),
        Some(Some(scope)) => {
            scoped = context.externals.scoped(scope);
            narrowed = Context {
                externals: &scoped,
                ..*context
            };
            &narrowed
        }
    };
    let document = TextDocument::new(DocumentId::new(path.to_string_lossy()), 0, text);
    let Ok(tree) = parser.parse(&document) else {
        return Ok(Vec::new());
    };
    // Error recovery can put a stray identifier where a type goes.
    if tree.has_errors() {
        return Ok(Vec::new());
    }
    let syntax = FileSyntax::build(&document, &tree);
    // A method or a field at the top of the file is a compact source file,
    // whose implicit `import module java.base` the resolver does not model.
    if syntax.declarations.iter().any(|decl| !is_type(decl.kind)) {
        return Ok(Vec::new());
    }
    let scopes = ScopeTree::build(&document, &tree);
    let externals = context.externals;
    let package = syntax.package.as_ref().map(|package| package.name.as_str());

    // A static on-demand import can bring in member types of any type it
    // names, which the resolver does not model; an on-demand import of a
    // package nothing holds means names this world cannot account for.
    let mut statically_imported: HashSet<&str> = HashSet::new();
    let mut found = Vec::new();
    for import in &syntax.imports {
        if import.is_static {
            if import.is_on_demand {
                return Ok(Vec::new());
            }
            if let Some(member) = import.name.rsplit('.').next() {
                statically_imported.insert(member);
            }
            continue;
        }
        if import.is_on_demand {
            if !externals.holds_prefix(&import.name) || generated_type(&import.name, externals) {
                return Ok(Vec::new());
            }
            continue;
        }
        // Any segment may be generated: a protobuf outer class's nested
        // message is imported through it.
        if !externals.knows_type(&import.name)
            && !removed(&import.name, true, context.level)
            && !import
                .name
                .match_indices('.')
                .any(|(at, _)| generated_type(&import.name[..at], externals))
            && !import
                .name
                .split('.')
                .any(|segment| exempt(segment, context))
        {
            found.push(Unresolved {
                name: import.name.clone(),
                range: import.name_span.range,
                kind: Kind::Type,
            });
        }
    }

    // Every type an unqualified name could be inherited from: the frames of
    // the closures of every type this file declares.
    let Some(frames) = frames(&syntax, package, externals) else {
        return Ok(Vec::new());
    };
    let silent_spans = local_and_anonymous_bodies(&syntax.declarations, &(0..u32::MAX));

    for occurrence in javelin_parser::type_occurrences(&document, &tree, &scopes, &syntax, token)? {
        token.check()?;
        if !occurrence.in_type_position || occurrence.fqn.is_some() {
            continue;
        }
        let name = occurrence.name.as_str();
        if name.contains('.') {
            if qualified_unresolved(
                name,
                &occurrence,
                &statically_imported,
                &silent_spans,
                &frames,
                package,
                &syntax,
                context,
            ) {
                found.push(Unresolved {
                    name: occurrence.name,
                    range: occurrence.span.range,
                    kind: Kind::Type,
                });
            }
            continue;
        }
        if CONTEXTUAL.contains(&name)
            || statically_imported.contains(name)
            || silent_spans.iter().any(|span| {
                span.start_byte <= occurrence.span.start_byte
                    && occurrence.span.end_byte <= span.end_byte
            })
            || frames
                .iter()
                .any(|frame| externals.knows_type(&format!("{frame}.{name}")))
        {
            continue;
        }
        // Last: with a processor on the classpath, `exempt` scans every
        // workspace type.
        if externals.resolution(name, package, &syntax.imports) == Resolution::Unknown
            && !removed(name, false, context.level)
            && !exempt(name, context)
        {
            found.push(Unresolved {
                name: occurrence.name,
                range: occurrence.span.range,
                kind: Kind::Type,
            });
        }
    }
    // Lombok's use of this file's own types, live and as the index holds it
    // — which knows a renaming `lombok.config`. `@ExtensionMethod` adds
    // methods to any receiver in its type's body; a type Lombok writes more
    // into than is generated leaves the types around an unqualified call
    // short, and so does one the index does not hold, since what Lombok
    // generates for this file's types arrives from there. A `@Helper` local
    // class's methods are callable unqualified in the block around it, and
    // no type the index holds says so.
    let live = syntax
        .named_types()
        .into_iter()
        .map(|named| javelin_classpath::lombok_members(named.decl, &[], &syntax.imports).lombok)
        .fold(LombokUse::default(), LombokUse::or);
    let mut stored = externals.workspace().types_in(path).peekable();
    let held = stored.peek().is_some();
    let stored = stored
        .map(|ty| ty.lombok)
        .fold(LombokUse::default(), LombokUse::or);
    if context.members && !live.extensions && !stored.extensions {
        let helper = document.text().contains("lombok.experimental.Helper")
            || document.text().contains("@Helper")
                && syntax.imports.iter().any(|import| {
                    !import.is_static && import.name.starts_with("lombok.experimental")
                });
        let unqualified = !(stored.unmodelled || (live.annotated && !held) || helper);
        let file = File {
            document: &document,
            tree: &tree,
            syntax: &syntax,
            scopes: &scopes,
        };
        found.extend(unresolved_members(
            &file,
            &frames,
            &statically_imported,
            &silent_spans,
            unqualified,
            context,
            token,
        )?);
    }
    Ok(found)
}

/// One parsed file, as the member pass reads it.
struct File<'a> {
    document: &'a TextDocument,
    tree: &'a javelin_parser::SyntaxTree,
    syntax: &'a FileSyntax,
    scopes: &'a ScopeTree,
}

/// Every member use in the file naming what certainly is not there —
/// `javelin_semantic::missing_member_at` — less what this file's own world
/// says the engine cannot see: a type name inheritance may shadow, a variable
/// a static import or a supertype brings in, a method a static import brings
/// in, a nested type a field access may mean, a member the JDK removed after
/// the project's level.
fn unresolved_members(
    file: &File<'_>,
    frames: &[String],
    statically_imported: &HashSet<&str>,
    silent_spans: &[Span],
    judge_unqualified: bool,
    context: &Context<'_>,
    token: &CancellationToken,
) -> Result<Vec<Unresolved>, Cancelled> {
    let files =
        javelin_semantic::Files::single(file.document.id().clone(), file.syntax, file.scopes);
    // One closure per receiver type per file, however many calls it takes.
    let cached = crate::member_refs::Recording::new(context.externals);
    // Every name a frame's closure has a variable of — the fields a type in
    // this file inherits, which the scope tree does not bind.
    let mut variables: Option<HashSet<String>> = None;
    let own: Vec<String> = file
        .syntax
        .named_types()
        .into_iter()
        .map(|named| named.fqn)
        .collect();
    let mut found = Vec::new();
    for span in javelin_parser::member_name_spans(file.document, file.tree, token)? {
        token.check()?;
        if silent_spans
            .iter()
            .any(|silent| silent.start_byte <= span.start_byte && span.end_byte <= silent.end_byte)
        {
            continue;
        }
        let Some(missing) = javelin_semantic::missing_member_at(
            file.document,
            file.tree,
            &files,
            &cached,
            span.start_byte,
        ) else {
            continue;
        };
        let unqualified = missing.used == javelin_semantic::MemberUse::Unqualified;
        if unqualified && !judge_unqualified {
            continue;
        }
        // What the JDK removed after the project's level is callable at it:
        // below the JDK's own level, no JDK type's list is the project's.
        // `Object` has lost no accessible method in any release. The types
        // around an unqualified call are this file's, and every frame of the
        // file's closures holds theirs.
        //
        // ponytail: so a Java 17 project on JDK 21 has no call on a JDK
        // receiver judged. The upgrade is the JDK's own per-release API
        // record, `lib/ct.sym`, read beside `src.zip`.
        if context.level < context.jdk
            && if unqualified {
                frames.iter().any(|fqn| is_jdk(fqn, context.externals))
            } else {
                reaches_the_jdk(&missing.receiver, &own, frames, context.externals)
            }
        {
            continue;
        }
        // This file's own types have the `private` members the lists leave
        // out of every other file's view. The types around an unqualified
        // call are walked from this file, `private` members and all.
        if !unqualified
            && own.iter().any(|fqn| {
                missing.receiver == *fqn || missing.receiver.starts_with(&format!("{fqn}."))
            })
        {
            continue;
        }
        // A single static import can bring the method in; an on-demand one
        // has already refused the file.
        if unqualified && statically_imported.contains(missing.name.as_str()) {
            continue;
        }
        // A field through a type name may be a nested type, which no closure
        // lists: any frame of the receiver's closure declaring one silences
        // it.
        if missing.used == (javelin_semantic::MemberUse::Field { through_type: true }) {
            let (_, reached) = context.externals.closure(&missing.receiver, false);
            if reached.iter().any(|frame| {
                context
                    .externals
                    .knows_type(&format!("{frame}.{}", missing.name))
            }) {
                continue;
            }
        }
        // A member type a frame declares is in scope by inheritance, and a
        // single static import can bring one in, and either shadows an import
        // or a top-level type of the same name, which the resolver does not
        // model; a generated type is one the walk never read.
        if missing.written.iter().any(|name| {
            statically_imported.contains(name.as_str())
                || lombok_inferred(name, &file.syntax.imports)
                || exempt(name, context)
                || frames
                    .iter()
                    .any(|frame| context.externals.knows_type(&format!("{frame}.{name}")))
        }) {
            continue;
        }
        if let Some(root) = &missing.type_name {
            let variables = variables.get_or_insert_with(|| {
                frames
                    .iter()
                    .filter_map(|frame| javelin_semantic::Externals::members(&cached, frame))
                    .flat_map(|closure| closure.members)
                    .filter(|member| member.kind != DeclKind::Method)
                    .map(|member| member.name.clone())
                    .collect()
            });
            if statically_imported.contains(root.as_str()) || variables.contains(root) {
                continue;
            }
        }
        let kind = match missing.used {
            javelin_semantic::MemberUse::Call => Kind::Method {
                receiver: missing.receiver,
            },
            javelin_semantic::MemberUse::Field { .. } => Kind::Field {
                receiver: missing.receiver,
            },
            javelin_semantic::MemberUse::Unqualified => Kind::Unqualified,
        };
        found.push(Unresolved {
            name: missing.name,
            range: missing.span.range,
            kind,
        });
    }
    Ok(found)
}

/// Whether `name` is Lombok's `val` or `var`, written where a type is and
/// inferring the local's type, which the engine reads as the annotation type.
fn lombok_inferred(name: &str, imports: &[javelin_parser::ImportDecl]) -> bool {
    const INFERRED: [&str; 3] = ["lombok.val", "lombok.var", "lombok.experimental.var"];
    INFERRED.iter().any(|fqn| {
        let (package, simple) = fqn.rsplit_once('.').unwrap_or_default();
        name == *fqn
            || name == simple
                && imports.iter().any(|import| {
                    !import.is_static
                        && import.name == if import.is_on_demand { package } else { fqn }
                })
    })
}

/// Whether `receiver`'s closure has a JDK frame besides `Object`. A type
/// this file declares — named by its simple name — is judged by every frame
/// of the file's closures, which holds its own.
fn reaches_the_jdk(
    receiver: &str,
    own: &[String],
    frames: &[String],
    externals: &ClasspathExternals,
) -> bool {
    let simple_own = own
        .iter()
        .any(|fqn| fqn.rsplit('.').next() == Some(receiver));
    let closure;
    let reached: &[String] = if simple_own {
        frames
    } else {
        closure = externals.closure(receiver, false).1;
        &closure
    };
    reached.iter().any(|fqn| is_jdk(fqn, externals))
}

/// Whether `fqn` is a JDK type whose list may have lost a member since the
/// project's level — any but `Object`.
fn is_jdk(fqn: &str, externals: &ClasspathExternals) -> bool {
    fqn != TypeWorld::OBJECT && externals.world().is_platform(fqn)
}

/// Whether a qualified type name certainly names nothing: its first segment
/// is no type the file can see by any rule — so the name is a package's —
/// no prefix of it is a type, whose inherited member types no fact lists,
/// and the whole is held nowhere.
#[allow(clippy::too_many_arguments)]
fn qualified_unresolved(
    name: &str,
    occurrence: &javelin_parser::TypeOccurrence,
    statically_imported: &HashSet<&str>,
    silent_spans: &[Span],
    frames: &[String],
    package: Option<&str>,
    syntax: &FileSyntax,
    context: &Context<'_>,
) -> bool {
    let externals = context.externals;
    let segments: Vec<&str> = name.split('.').collect();
    let first = segments[0];
    let silent = statically_imported.contains(first)
        || silent_spans.iter().any(|span| {
            span.start_byte <= occurrence.span.start_byte
                && occurrence.span.end_byte <= span.end_byte
        })
        || frames
            .iter()
            .any(|frame| externals.knows_type(&format!("{frame}.{first}")));
    if silent
        || externals.resolution(first, package, &syntax.imports) != Resolution::Unknown
        || (1..segments.len()).any(|end| externals.knows_type(&segments[..end].join(".")))
        || segments.iter().any(|segment| exempt(segment, context))
    {
        return false;
    }
    externals.resolution(name, package, &syntax.imports) == Resolution::Unknown
        && !removed(name, true, context.level)
}

/// Whether a name may be one a build generates: a generated source the walk
/// saw by its file name, or — with an annotation processor on the classpath —
/// a name built around a workspace type's, as `ImmutableFoo`, `Foo_` and
/// `DaggerFooComponent` are.
///
/// ponytail: the second rule silences a hallucinated `FooHelper` beside a
/// real `Foo` wherever a processor is on the classpath. The upgrade is
/// knowing which annotations each processor claims, and which types carry
/// them.
fn exempt(name: &str, context: &Context<'_>) -> bool {
    if context.generated.contains(name) {
        return true;
    }
    context.processors
        && context.externals.workspace().types().iter().any(|ty| {
            ty.simple.len() >= 3 && ty.simple != name && name.contains(ty.simple.as_str())
        })
}

/// Whether `fqn` is a type the index read in a generator's or a
/// processor's output, whose member types are what the last build left:
/// a `.proto` message added since is a member type no file holds yet.
fn generated_type(fqn: &str, externals: &ClasspathExternals) -> bool {
    externals
        .workspace()
        .with_fqn(fqn)
        .any(|ty| javelin_index::is_processor_output(&ty.file))
}

/// Every type in the supertype closures of the types this file declares,
/// themselves included — or `None` when a closure cannot be shown whole,
/// since a member type inherited from a type it does not reach is a name
/// this file may write unqualified.
pub(crate) fn frames(
    syntax: &FileSyntax,
    package: Option<&str>,
    externals: &ClasspathExternals,
) -> Option<Vec<String>> {
    let named = syntax.named_types();
    let own: HashMap<&str, &Decl> = named
        .iter()
        .map(|named| (named.fqn.as_str(), named.decl))
        .collect();
    let mut by_simple: HashMap<&str, Vec<&str>> = HashMap::new();
    for named in &named {
        by_simple
            .entry(named.decl.name.as_str())
            .or_default()
            .push(named.fqn.as_str());
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = named.iter().map(|named| named.fqn.clone()).collect();
    while let Some(fqn) = queue.pop_front() {
        if !seen.insert(fqn.clone()) {
            continue;
        }
        if seen.len() > javelin_classpath::TypeWorld::MAX_VISITS * 4 {
            return None;
        }
        let supertypes: Vec<String> = if let Some(decl) = own.get(fqn.as_str()) {
            // This buffer's own declaration, as written now: resolved against
            // the file, its own declarations first.
            let mut resolved = Vec::new();
            for written in decl.extends.iter().chain(&decl.implements) {
                let name = javelin_classpath::written_type_name(written);
                let first = name.split('.').next().unwrap_or(&name);
                let supertype = match by_simple.get(first).map(Vec::as_slice) {
                    Some([one]) if !name.contains('.') => (*one).to_owned(),
                    Some(_) => return None,
                    None => match externals.resolution(&name, package, &syntax.imports) {
                        Resolution::One(fqn) => fqn,
                        Resolution::Ambiguous | Resolution::Unknown => return None,
                    },
                };
                resolved.push(supertype);
            }
            resolved.extend(implicit(decl.kind));
            resolved
        } else {
            let mut declared = externals.workspace().with_fqn(&fqn);
            match (declared.next(), declared.next()) {
                (Some(ty), None) => {
                    let (mut supertypes, complete) = externals.source_supertypes(ty);
                    if !complete {
                        return None;
                    }
                    supertypes.extend(implicit(ty.kind));
                    supertypes
                }
                (Some(_), Some(_)) => return None,
                (None, _) => {
                    let facts = externals.world().facts(&fqn)?;
                    // The source reader drops what it cannot resolve; an
                    // enum's, a record's and an annotation type's unwritten
                    // supertype is the only drop it makes on purpose.
                    if !facts.supertypes_complete
                        && !matches!(
                            facts.kind,
                            DeclKind::Enum | DeclKind::Record | DeclKind::AnnotationType
                        )
                    {
                        return None;
                    }
                    let mut supertypes = facts.supertypes.clone();
                    supertypes.extend(implicit(facts.kind));
                    supertypes
                }
            }
        };
        queue.extend(supertypes);
    }
    Some(seen.into_iter().collect())
}

fn is_type(kind: DeclKind) -> bool {
    matches!(
        kind,
        DeclKind::Class
            | DeclKind::Interface
            | DeclKind::Enum
            | DeclKind::Record
            | DeclKind::AnnotationType
    )
}

/// Whether `path` belongs to a build the dependency reader did not ask for:
/// `buildSrc` and an included build — one with a `settings.gradle` of its own
/// below the root — compile against classpaths of their own, the Gradle API
/// among them, and so does a Maven Invoker project: a build file of its own
/// in a directory of `src/it`.
///
/// A standalone Maven build nested below the root is caught by the modules
/// the tool answered for — it is none of them — and judged against the
/// root's classpath only when the tool said nothing per module.
fn in_unasked_build(path: &Path, roots: &[std::path::PathBuf]) -> bool {
    let Some(root) = roots.iter().find(|root| path.starts_with(root)) else {
        return true;
    };
    let Ok(relative) = path.strip_prefix(root) else {
        return true;
    };
    let names: Vec<&str> = relative
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    if names.contains(&"buildSrc") {
        return true;
    }
    path.ancestors()
        .skip(1)
        .take_while(|ancestor| *ancestor != root.as_path())
        .any(|ancestor| {
            let invoker = ancestor.parent().is_some_and(|it| it.ends_with("src/it"))
                && ["pom.xml", "build.gradle", "build.gradle.kts"]
                    .iter()
                    .any(|build| ancestor.join(build).is_file());
            invoker
                || ancestor.join("settings.gradle").is_file()
                || ancestor.join("settings.gradle.kts").is_file()
        })
}

/// The supertype a declaration of `kind` has without writing it, beyond
/// `Object` — which declares no member type.
fn implicit(kind: DeclKind) -> Option<String> {
    match kind {
        DeclKind::Enum => Some("java.lang.Enum".to_owned()),
        DeclKind::Record => Some("java.lang.Record".to_owned()),
        DeclKind::AnnotationType => Some("java.lang.annotation.Annotation".to_owned()),
        _ => None,
    }
}

/// The bodies of every anonymous class and every class declared inside a
/// method, a constructor, an initialiser or a field: their supertypes'
/// member types are in scope there and are not walked, so nothing inside is
/// judged.
pub(crate) fn local_and_anonymous_bodies(
    declarations: &[Decl],
    within: &std::ops::Range<u32>,
) -> Vec<Span> {
    fn visit(decl: &Decl, inside_code: bool, within: &std::ops::Range<u32>, out: &mut Vec<Span>) {
        if decl.span.end_byte < within.start || decl.span.start_byte > within.end {
            return;
        }
        let is_type = is_type(decl.kind);
        if decl.kind == DeclKind::AnonymousClass || (is_type && inside_code) {
            out.push(decl.span);
            return;
        }
        let code = !is_type;
        for child in &decl.children {
            visit(child, code, within, out);
        }
    }
    let mut out = Vec::new();
    for decl in declarations {
        visit(decl, false, within, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classpath::Classpath;
    use crate::workspace::testing::{Sources, indexed};
    use std::sync::Arc;

    fn fact(fqn: &str, kind: DeclKind, supertypes: &[&str]) -> javelin_classpath::TypeFacts {
        javelin_classpath::TypeFacts {
            constructors: Vec::new(),
            doc_span: None,
            name_start: 0,
            name_end: 0,
            fqn: fqn.to_owned(),
            simple: fqn.rsplit('.').next().unwrap_or(fqn).to_owned(),
            kind,
            type_parameters: Vec::new(),
            supertype_arguments: Vec::new(),
            supertypes: supertypes.iter().map(|s| (*s).to_owned()).collect(),
            supertypes_complete: true,
            members_complete: true,
            members: Vec::new(),
            source: String::new(),
        }
    }

    fn method(name: &str) -> javelin_classpath::MemberFacts {
        javelin_classpath::MemberFacts {
            type_parameters: Vec::new(),
            doc_span: None,
            parameter_names: Vec::new(),
            name: name.to_owned(),
            kind: DeclKind::Method,
            is_static: false,
            is_abstract: false,
            is_public: true,
            declared: javelin_classpath::MemberType::Void,
            parameters: Vec::new(),
            variadic: false,
            name_start: 0,
            name_end: 0,
        }
    }

    /// A small JDK: enough of `java.lang` and `java.util` to resolve against.
    fn jdk() -> Vec<javelin_classpath::TypeFacts> {
        let mut string = fact("java.lang.String", DeclKind::Class, &["java.lang.Object"]);
        string.members = vec![method("length")];
        let mut java_enum = fact("java.lang.Enum", DeclKind::Class, &["java.lang.Object"]);
        java_enum.members = vec![method("name")];
        vec![
            fact("java.lang.Object", DeclKind::Class, &[]),
            string,
            fact("java.lang.Override", DeclKind::AnnotationType, &[]),
            java_enum,
            fact("java.lang.Record", DeclKind::Class, &["java.lang.Object"]),
            fact("java.lang.annotation.Annotation", DeclKind::Interface, &[]),
            fact("java.util.List", DeclKind::Interface, &[]),
            fact("java.util.Map", DeclKind::Interface, &[]),
            fact("java.util.Map.Entry", DeclKind::Interface, &[]),
            fact(
                "java.util.HashMap",
                DeclKind::Class,
                &["java.lang.Object", "java.util.Map"],
            ),
        ]
    }

    fn judge_with(
        sources: &Sources,
        text: &str,
        generated: &[&str],
        processors: bool,
    ) -> Vec<String> {
        judge_at(sources, "p/Judged.java", text, generated, processors)
    }

    fn judge_at(
        sources: &Sources,
        relative: &str,
        text: &str,
        generated: &[&str],
        processors: bool,
    ) -> Vec<String> {
        judge_full(
            sources,
            relative,
            text,
            generated,
            processors,
            u32::MAX,
            &BTreeSet::new(),
        )
    }

    fn judge_full(
        sources: &Sources,
        relative: &str,
        text: &str,
        generated: &[&str],
        processors: bool,
        level: u32,
        unasked: &BTreeSet<std::path::PathBuf>,
    ) -> Vec<String> {
        run(
            sources,
            text,
            Case {
                relative,
                generated,
                processors,
                level,
                unasked: unasked.clone(),
                ..Case::default()
            },
        )
    }

    /// Everything a judgement reads besides the workspace and the text.
    struct Case<'a> {
        relative: &'a str,
        generated: &'a [&'a str],
        processors: bool,
        level: u32,
        /// The JDK's feature version.
        jdk: u32,
        unasked: BTreeSet<std::path::PathBuf>,
        members: bool,
        /// Dependency facts, laid after the JDK's.
        jars: Vec<javelin_classpath::TypeFacts>,
        /// What each module compiles against.
        modules: Option<Arc<crate::modules::Modules>>,
    }

    impl Default for Case<'_> {
        fn default() -> Self {
            Self {
                relative: "p/Judged.java",
                generated: &[],
                processors: false,
                level: u32::MAX,
                jdk: 21,
                unasked: BTreeSet::new(),
                members: true,
                jars: Vec::new(),
                modules: None,
            }
        }
    }

    fn run(sources: &Sources, text: &str, case: Case<'_>) -> Vec<String> {
        found(sources, text, case)
            .into_iter()
            .map(|found| found.name)
            .collect()
    }

    fn found(sources: &Sources, text: &str, case: Case<'_>) -> Vec<Unresolved> {
        let world = Classpath::holding(jdk()).world.extended(case.jars);
        let externals = ClasspathExternals::new(Arc::new(world), Arc::clone(&sources.index));
        let generated: BTreeSet<String> = case
            .generated
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        let context = Context {
            externals: &externals,
            modules: case.modules.as_ref(),
            generated: &generated,
            processors: case.processors,
            roots: std::slice::from_ref(&sources.root),
            unasked: &case.unasked,
            level: case.level,
            jdk: case.jdk,
            members: case.members,
        };
        unresolved(
            &sources.path(case.relative),
            text,
            &mut JavaParser::new().expect("grammar"),
            &context,
            &CancellationToken::new(),
        )
        .expect("not cancelled")
    }

    fn judge(sources: &Sources, text: &str) -> Vec<String> {
        judge_with(sources, text, &[], false)
    }

    fn workspace() -> Sources {
        indexed(
            "diagnostics",
            &[
                (
                    "p/Base.java",
                    "package p;\npublic class Base { public class Inner {} }\n",
                ),
                ("p/Foo.java", "package p;\npublic class Foo {}\n"),
                ("a/Thing.java", "package a;\npublic class Thing {}\n"),
                ("b/Thing.java", "package b;\npublic class Thing {}\n"),
            ],
        )
    }

    #[test]
    fn a_type_nothing_declares_is_reported_and_everything_else_is_not() {
        let sources = workspace();
        assert_eq!(
            judge(
                &sources,
                "package p;\nimport java.util.List;\nclass Judged {\n    Missing m;\n    \
                 String s;\n    List<String> l;\n    Foo f;\n    @Override public String toString() { return s; }\n}\n",
            ),
            vec!["Missing"]
        );
    }

    /// A qualified name is judged only where a package qualifies it: any
    /// part of it a type may have the rest as an inherited member type —
    /// `HashMap.Entry` is `Map.Entry` — which no fact lists.
    #[test]
    fn a_qualified_name_is_reported_only_when_a_package_qualifies_it() {
        let sources = workspace();
        assert_eq!(
            judge(
                &sources,
                "package p;\nimport java.util.Map;\nclass Judged {\n    java.util.Lisst a;\n    \
                 com.acme.Missing b;\n    java.util.List c;\n    p.Foo d;\n    \
                 java.util.HashMap.Entry e;\n    Map.Entryy f;\n    p.Base.Gone g;\n    \
                 Base.Inner h;\n    @java.lang.Overide void m() {}\n}\n",
            ),
            vec!["java.util.Lisst", "com.acme.Missing", "java.lang.Overide"]
        );
        // A first segment some rule other than the tiers may make a type:
        // a static import, an inherited member type; and where nothing is
        // judged, or a segment is a generated name.
        for text in [
            "package p;\nimport static p.Base.Inner;\nclass Judged { Inner.Deep x; }\n",
            "package p;\nclass Judged extends Base { Inner.Deep x; }\n",
            "package p;\nclass Judged { Object o = new Object() { com.acme.Missing m; }; }\n",
        ] {
            assert_eq!(judge(&sources, text), Vec::<String>::new(), "{text}");
        }
        assert_eq!(
            judge_with(
                &sources,
                "package p;\nclass Judged { com.acme.Gen g; }\n",
                &["Gen"],
                false
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn an_ambiguous_name_is_not_unknown() {
        let sources = workspace();
        assert_eq!(
            judge(
                &sources,
                "package p;\nimport a.*;\nimport b.*;\nclass Judged { Thing t; Missing m; }\n"
            ),
            vec!["Missing"]
        );
    }

    #[test]
    fn an_inherited_member_type_is_not_reported() {
        let sources = workspace();
        assert_eq!(
            judge(
                &sources,
                "package p;\nclass Judged extends Base { Inner i; Missing m; }\n"
            ),
            vec!["Missing"]
        );
        assert_eq!(
            judge(
                &sources,
                "package p;\nclass Judged implements java.util.Map { Entry e; Missing m; }\n"
            ),
            vec!["Missing"]
        );
    }

    #[test]
    fn a_file_that_cannot_be_judged_gets_nothing() {
        let sources = workspace();
        for text in [
            // A parse error.
            "package p;\nclass Judged { Missing m;\n",
            // A static on-demand import.
            "package p;\nimport static java.util.Map.*;\nclass Judged { Missing m; }\n",
            // An on-demand import of a package nothing holds.
            "package p;\nimport org.nowhere.*;\nclass Judged { Missing m; }\n",
            // A supertype that resolves to nothing: whatever it declares is
            // in scope unqualified.
            "package p;\nclass Judged extends Nowhere { Missing m; }\n",
        ] {
            assert_eq!(judge(&sources, text), Vec::<String>::new(), "{text}");
        }
    }

    #[test]
    fn an_exempt_name_is_silent() {
        let sources = workspace();
        assert_eq!(
            judge(
                &sources,
                "package p;\nimport static a.Thing.Member;\nclass Judged {\n    Member one;\n    \
                 void m() { var x = 1; Object o = new Object() { Missing inside; }; \
                 class Local { Missing local; } logger.info(); }\n    Missing outside;\n}\n",
            ),
            vec!["Missing"],
            "the static import's name, `var`, the anonymous and local bodies and a \
             receiver that may be an inherited field are silent"
        );
        assert_eq!(
            judge_with(
                &sources,
                "package p;\nclass Judged { ImmutableFoo i; }\n",
                &["ImmutableFoo"],
                false
            ),
            Vec::<String>::new(),
            "a generated source the walk saw"
        );
        assert_eq!(
            judge_with(
                &sources,
                "package p;\nclass Judged { FooBuilder b; }\n",
                &[],
                true
            ),
            Vec::<String>::new(),
            "with a processor on the classpath, a name built on a workspace type's"
        );
        assert_eq!(
            judge_with(
                &sources,
                "package p;\nclass Judged { FooBuilder b; }\n",
                &[],
                false
            ),
            vec!["FooBuilder"],
            "and without one, it is reported"
        );
    }

    /// A protobuf outer class is generated, and its messages are imported
    /// through it.
    #[test]
    fn an_import_through_a_generated_type_is_silent() {
        let sources = workspace();
        assert_eq!(
            judge_with(
                &sources,
                "package p;\nimport com.acme.AddressBookProtos.Person;\nclass Judged { Person p; }\n",
                &["AddressBookProtos"],
                false
            ),
            Vec::<String>::new()
        );
    }

    /// An outer class the index read in a generator's output may be older
    /// than the `.proto` it came from, so a message imported through it is
    /// not known absent.
    #[test]
    fn a_member_type_of_a_generated_type_the_index_read_is_not_judged_absent() {
        let sources = indexed(
            "diagnostics-proto",
            &[
                (
                    "target/generated-sources/protobuf/java/com/acme/AddressBookProtos.java",
                    "package com.acme;\npublic final class AddressBookProtos {\n    \
                     public static final class Person {}\n}\n",
                ),
                ("p/Foo.java", "package p;\npublic class Foo {}\n"),
            ],
        );
        assert_eq!(
            judge(
                &sources,
                "package p;\nimport com.acme.AddressBookProtos.Refund;\nimport com.acme.Gone;\n\
                 import p.Foo.Nope;\nclass Judged {}\n"
            ),
            vec!["com.acme.Gone", "p.Foo.Nope"],
            "a single import through it"
        );
        assert_eq!(
            judge(
                &sources,
                "package p;\nimport com.acme.AddressBookProtos.*;\nclass Judged { Refund r; }\n"
            ),
            Vec::<String>::new(),
            "an on-demand import of it"
        );
    }

    /// A compact source file imports `java.base` whole, without writing it.
    #[test]
    fn a_compact_source_file_is_not_judged() {
        let sources = workspace();
        assert_eq!(
            judge(
                &sources,
                "void main() { List<String> l = null; Missing m; }\n"
            ),
            Vec::<String>::new()
        );
    }

    /// `buildSrc`, an included build and a Maven Invoker project compile
    /// against classpaths the root's build does not print.
    #[test]
    fn a_file_of_a_build_nobody_asked_is_not_judged() {
        let sources = workspace();
        let text = "package p;\nclass Judged { Plugin p; }\n";
        assert_eq!(
            judge_at(&sources, "p/Judged.java", text, &[], false),
            vec!["Plugin"],
            "the root's own build is judged"
        );
        std::fs::create_dir_all(sources.path("logic")).expect("dir");
        std::fs::write(sources.path("logic/settings.gradle"), "").expect("settings");
        std::fs::create_dir_all(sources.path("src/it/simple")).expect("dir");
        std::fs::write(sources.path("src/it/simple/pom.xml"), "<project/>").expect("pom");
        assert_eq!(
            judge_at(&sources, "src/it/java/p/Judged.java", text, &[], false),
            vec!["Plugin"],
            "a Gradle integration-test source set is the root's own"
        );
        for relative in [
            "buildSrc/src/main/java/p/Judged.java",
            "logic/src/main/java/p/Judged.java",
            "src/it/simple/src/main/java/p/Judged.java",
        ] {
            assert_eq!(
                judge_at(&sources, relative, text, &[], false),
                Vec::<String>::new(),
                "{relative}"
            );
        }
    }

    /// A project at Java 17 on a JDK 21 world can name `java.lang.Compiler`,
    /// which 21 removed; one whose level is unknown can name anything a JDK
    /// removed; one this JDK compiles, nothing.
    #[test]
    fn what_the_jdk_removed_after_the_projects_level_is_silent() {
        let sources = workspace();
        let text = "package p;\nimport javax.xml.bind.JAXBContext;\n\
                    class Judged { Compiler c; JAXBContext j; }\n";
        let at = |level| {
            judge_full(
                &sources,
                "p/Judged.java",
                text,
                &[],
                false,
                level,
                &BTreeSet::new(),
            )
        };
        assert_eq!(at(u32::MAX), vec!["javax.xml.bind.JAXBContext", "Compiler"]);
        assert_eq!(at(17), vec!["javax.xml.bind.JAXBContext"]);
        assert_eq!(at(0), Vec::<String>::new());
    }

    /// Below a directory holding a build nobody asked, nothing is judged.
    #[test]
    fn a_file_under_an_unasked_build_is_not_judged() {
        let sources = workspace();
        let text = "package p;\nclass Judged { Missing m; }\n";
        let unasked: BTreeSet<_> = [sources.path("legacy")].into();
        assert_eq!(
            judge_full(
                &sources,
                "legacy/p/Judged.java",
                text,
                &[],
                false,
                u32::MAX,
                &unasked
            ),
            Vec::<String>::new()
        );
        assert_eq!(
            judge_full(
                &sources,
                "p/Judged.java",
                text,
                &[],
                false,
                u32::MAX,
                &unasked
            ),
            vec!["Missing"]
        );
    }

    /// Each file is judged against its own module's JARs: a type only
    /// another module's JAR holds is unknown to it, and a file no module
    /// claims — or one the tool did not answer for — is not judged.
    #[test]
    fn a_file_sees_only_its_own_modules_jars() {
        let sources = workspace();
        let text = "package p;\nimport org.mockito.Mockito;\nimport com.google.Guava;\n\
                    class Judged { Mockito m; Guava g; Missing x; String s; }\n";
        let guava = fact("com.google.Guava", DeclKind::Class, &["java.lang.Object"]);
        let mockito = fact(
            "org.mockito.Mockito",
            DeclKind::Class,
            &["java.lang.Object"],
        );
        // Vendored by the other module's JAR, and still the JDK's.
        let string = fact("java.lang.String", DeclKind::Class, &["java.lang.Object"]);
        let module = |source: &str, jars: Option<Vec<usize>>| javelin_build::Module {
            sources: vec![sources.path(source)],
            jars,
        };
        let modules = Arc::new(crate::modules::Modules::new(
            [
                (0, std::slice::from_ref(&guava)),
                (1, &[mockito.clone(), string][..]),
            ],
            vec![module("core", Some(vec![0])), module("web", None)],
        ));
        let judged = |relative: &str, modules: Option<Arc<crate::modules::Modules>>| {
            run(
                &sources,
                text,
                Case {
                    relative,
                    jars: vec![guava.clone(), mockito.clone()],
                    modules,
                    ..Case::default()
                },
            )
        };
        assert_eq!(
            judged("core/p/Judged.java", Some(Arc::clone(&modules))),
            vec!["org.mockito.Mockito", "Missing"]
        );
        assert_eq!(
            judged("core/p/Judged.java", None),
            vec!["Missing"],
            "one classpath for the whole build holds both"
        );
        assert_eq!(
            judged("web/p/Judged.java", Some(Arc::clone(&modules))),
            Vec::<String>::new(),
            "a module the tool did not answer for"
        );
        assert_eq!(
            judged("tools/p/Judged.java", Some(modules)),
            Vec::<String>::new(),
            "no module claims it"
        );
    }

    /// A workspace with a type of no members, one extending a type that
    /// declares a field named like a type, one nested type a top-level type
    /// shares a name with, and a file that does not parse.
    fn member_workspace() -> Sources {
        indexed(
            "member-diagnostics",
            &[
                ("p/Foo.java", "package p;\npublic class Foo {}\n"),
                (
                    "p/Shade.java",
                    "package p;\npublic class Shade { protected Foo Foo; }\n",
                ),
                (
                    "p/Base.java",
                    "package p;\npublic class Base { public class Inner {} }\n",
                ),
                ("p/Inner.java", "package p;\npublic class Inner {}\n"),
                (
                    "p/Broken.java",
                    "package p;\npublic class Broken { void a( }\n",
                ),
                ("a/Thing.java", "package a;\npublic class Thing {}\n"),
                (
                    "p/Caller.java",
                    "package p;\npublic class Caller { public void Foo() {} }\n",
                ),
            ],
        )
    }

    fn judge_calls(text: &str) -> Vec<String> {
        judge(&member_workspace(), text)
    }

    #[test]
    fn a_method_nothing_declares_is_reported_on_a_jdk_and_a_workspace_receiver() {
        assert_eq!(
            judge_calls(
                "package p;\nclass Judged { String s; Foo f;\n    \
                 void m() { s.length(); s.nope(); f.gone(); Foo.none(); } }\n"
            ),
            vec!["nope", "gone", "none"]
        );
    }

    /// Under Lombok, a receiver is judged where everything Lombok writes
    /// into it is generated; a file with `@ExtensionMethod` is not judged;
    /// and an unqualified call is judged only in a file whose own Lombok
    /// types the index holds, all of them modelled.
    #[test]
    fn calls_under_lombok_are_judged_where_what_it_writes_is_known() {
        let held = "package p;\nimport lombok.Data;\n\
                    @Data public class Held { String name; void m() { getName(); helpr(); } }\n";
        let built = "package p;\nimport lombok.experimental.SuperBuilder;\n\
                     @SuperBuilder(builderMethodName = \"make\") public class Built {\n    \
                     void m() { make(); helpr(); } }\n";
        let sources = indexed(
            "lombok-diagnostics",
            &[
                (
                    "p/Car.java",
                    "package p;\nimport lombok.Data;\n@Data public class Car { private String name; }\n",
                ),
                (
                    "p/Sup.java",
                    "package p;\nimport lombok.experimental.SuperBuilder;\n\
                     @SuperBuilder(builderMethodName = \"make\") public class Sup { int a; }\n",
                ),
                (
                    "p/Root.java",
                    "package p;\nimport lombok.experimental.SuperBuilder;\n\
                     @SuperBuilder public abstract class Root { int a; }\n",
                ),
                (
                    "p/Leaf.java",
                    "package p;\nimport lombok.experimental.SuperBuilder;\n\
                     @SuperBuilder public class Leaf extends Root { int b; }\n",
                ),
                (
                    "p/Twig.java",
                    "package p;\nimport lombok.experimental.SuperBuilder;\n\
                     @SuperBuilder public class Twig extends Sup { int c; }\n",
                ),
                ("p/Held.java", held),
                ("p/Built.java", built),
            ],
        );
        let lombok: Vec<_> = [
            "lombok.Data",
            "lombok.experimental.SuperBuilder",
            "lombok.experimental.ExtensionMethod",
            "lombok.experimental.Helper",
            "lombok.val",
        ]
        .into_iter()
        .map(|fqn| fact(fqn, DeclKind::AnnotationType, &[]))
        .collect();
        let judged = |relative: &str, text: &str| {
            run(
                &sources,
                text,
                Case {
                    relative,
                    jars: lombok.clone(),
                    ..Case::default()
                },
            )
        };
        assert_eq!(
            judged(
                "p/Judged.java",
                "package p;\nclass Judged { Car car; Sup sup; String s;\n    \
                 void m() { car.getName(); car.getNmae(); sup.anything(); s.nope(); } }\n"
            ),
            vec!["getNmae", "nope"]
        );
        assert_eq!(
            judged(
                "p/Judged.java",
                "package p;\nclass Judged { void m() {\n    \
                 Leaf.builder().b(1).a(2).build().nope();\n    \
                 Leaf.builder().b(1).a(2).gone();\n    \
                 Twig.builder().c(1).a(2); Twig.builder().c(1).gone(); } }\n"
            ),
            vec!["nope", "gone"],
            "a `@SuperBuilder` chain sets the parent's fields after the child's, and \
             a builder whose parent's is not modelled is not judged"
        );
        assert_eq!(
            judged(
                "p/Judged.java",
                "package p;\nimport lombok.experimental.ExtensionMethod;\n\
                 @ExtensionMethod(Object.class) class Judged { String s; void m() { s.nope(); } }\n"
            ),
            Vec::<String>::new(),
            "any receiver may have an extension method"
        );
        assert_eq!(judged("p/Held.java", held), vec!["helpr"]);
        assert_eq!(
            judged("p/Unheld.java", &held.replace("Held", "Unheld")),
            Vec::<String>::new(),
            "what Lombok writes for a file the index does not hold is unknown"
        );
        assert_eq!(judged("p/Built.java", built), Vec::<String>::new());
        for import in ["import lombok.val;", "import lombok.*;"] {
            assert_eq!(
                judged(
                    "p/Judged.java",
                    &format!(
                        "package p;\n{import}\n\
                         class Judged {{ void m() {{ val s = \"x\"; s.trim(); s.nope(); }} }}\n"
                    )
                ),
                Vec::<String>::new(),
                "`val` is Lombok's inferred local, not a type: {import}"
            );
        }
        assert_eq!(
            judged(
                "p/Judged.java",
                "package p;\nimport lombok.*;\nimport lombok.experimental.*;\n\
                 @ExtensionMethod(Object.class) class Judged { String s; void m() { s.nope(); } }\n"
            ),
            Vec::<String>::new(),
            "an extension method under two on-demand imports"
        );
        assert_eq!(
            judged(
                "p/Judged.java",
                "package p;\nimport lombok.experimental.Helper;\nclass Judged { int f(int x) {\n    \
                 @Helper class L { int sq(int y) { return y; } } return sq(x); } }\n"
            ),
            Vec::<String>::new(),
            "a `@Helper` local class's methods are callable unqualified"
        );
    }

    /// An enum's `values()` and `valueOf` and a record's accessors are
    /// members, so a receiver of either can be judged, and so can an
    /// unqualified call inside one.
    #[test]
    fn an_enum_or_a_record_is_judged_with_what_java_declares_into_it() {
        let entry = "package p;\npublic record Entry(String date) {\n    \
                     void m() { date(); helpr(); } }\n";
        // A constant's body is an anonymous class whose methods are no list's.
        let shape = "package p;\npublic enum Shape {\n    CIRCLE {\n        \
                     double area() { return square(this.side()); }\n        \
                     double square(double r) { return r; } };\n    \
                     abstract double area();\n    double side() { return 1; }\n    \
                     void m() { nope(); } }\n";
        let sources = indexed(
            "implicit-diagnostics",
            &[
                ("p/Day.java", "package p;\npublic enum Day { MON }\n"),
                ("p/Entry.java", entry),
                ("p/Shape.java", shape),
            ],
        );
        let judged = |relative, text: &str| {
            run(
                &sources,
                text,
                Case {
                    relative,
                    ..Case::default()
                },
            )
        };
        assert_eq!(
            judged(
                "p/Judged.java",
                "package p;\nclass Judged { Day day; Entry entry;\n    void m() { \
                 Day.valueOf(\"x\"); Day.values(); day.name(); entry.date(); \
                 day.nope(); entry.nope(); } }\n"
            ),
            vec!["nope", "nope"]
        );
        assert_eq!(judged("p/Entry.java", entry), vec!["helpr"]);
        assert_eq!(judged("p/Shape.java", shape), vec!["nope"]);
    }

    #[test]
    fn calls_are_silent_where_the_member_half_is_withheld() {
        let text = "package p;\nclass Judged { String s; void m() { s.nope(); } }\n";
        let at = |members| {
            run(
                &member_workspace(),
                text,
                Case {
                    members,
                    ..Case::default()
                },
            )
        };
        assert_eq!(at(true), vec!["nope"]);
        assert_eq!(at(false), Vec::<String>::new());
    }

    /// Inside `new Base() { … }`, `Inner` is `Base.Inner`, and the file's
    /// closures do not reach an anonymous class's supertype.
    #[test]
    fn a_call_inside_an_anonymous_or_local_class_is_silent() {
        assert_eq!(
            judge_calls(
                "package p;\nclass Judged { void m() {\n    \
                 Object o = new Base() { void x(Inner u) { u.nope(); } };\n    \
                 class Local extends Base { void y(Inner u) { u.nope(); } } } }\n"
            ),
            Vec::<String>::new()
        );
    }

    /// `Foo` in `Foo.none()` is the field `Shade` declares, a variable, and
    /// shadows the type; so is a name a static import brings in.
    #[test]
    fn a_type_name_a_variable_may_shadow_is_silent() {
        assert_eq!(
            judge_calls("package p;\nclass Judged extends Shade { void m() { Foo.none(); } }\n"),
            Vec::<String>::new(),
            "an inherited field"
        );
        assert_eq!(
            judge_calls(
                "package p;\nimport static a.Thing.Foo;\nclass Judged { void m() { Foo.none(); } }\n"
            ),
            Vec::<String>::new(),
            "a static import"
        );
        assert_eq!(
            judge_calls("package p;\nclass Judged extends Caller { void m() { Foo.none(); } }\n"),
            vec!["none"],
            "a method is no variable, and shadows nothing"
        );
    }

    /// Inside `Judged extends Base`, `Inner` is `Base.Inner`, inherited, and
    /// not the top-level `p.Inner` the resolver would pick.
    #[test]
    fn a_receiver_type_inheritance_may_shadow_is_silent() {
        assert_eq!(
            judge_calls(
                "package p;\nclass Judged extends Base { Inner i; void m() { i.nope(); } }\n"
            ),
            Vec::<String>::new()
        );
        assert_eq!(
            judge_calls("package p;\nclass Judged { Inner i; void m() { i.nope(); } }\n"),
            vec!["nope"],
            "without the supertype, `Inner` is the top-level one"
        );
    }

    #[test]
    fn a_receiver_declared_in_a_file_that_does_not_parse_is_silent() {
        assert_eq!(
            judge_calls("package p;\nclass Judged { Broken b; void m() { b.nope(); } }\n"),
            Vec::<String>::new()
        );
    }

    /// A project older than the JDK can call what the JDK removed since, on
    /// any JDK type its receiver's closure reaches — `Object` aside, which no
    /// release has taken an accessible method from.
    #[test]
    fn a_receiver_reaching_the_jdk_is_silent_below_the_jdks_own_level() {
        let text = "package p;\nclass Judged { String s; Foo f;\n    \
                    void m() { s.nope(); f.gone(); } }\n";
        let at = |level, jdk| {
            run(
                &member_workspace(),
                text,
                Case {
                    level,
                    jdk,
                    ..Case::default()
                },
            )
        };
        assert_eq!(at(u32::MAX, 21), vec!["nope", "gone"], "a loose project");
        assert_eq!(at(21, 21), vec!["nope", "gone"], "the JDK's own level");
        assert_eq!(at(17, 21), vec!["gone"], "an older level");
        assert_eq!(at(0, 21), vec!["gone"], "an unknown level");
        assert_eq!(at(21, u32::MAX), vec!["gone"], "an unknown JDK");
    }

    /// A processor's output is whatever the last build left: a type in it
    /// is navigable, and nothing is judged absent from a closure reaching it.
    #[test]
    fn a_closure_reaching_a_processors_output_is_never_judged() {
        let sources = indexed(
            "processor-closure",
            &[
                (
                    "build/generated/sources/annotationProcessor/java/main/p/CarMapperImpl.java",
                    "package p;\npublic class CarMapperImpl { public void map() {} }\n",
                ),
                (
                    "p/Written.java",
                    "package p;\npublic class Written { public void map() {} }\n",
                ),
            ],
        );
        assert_eq!(
            judge(
                &sources,
                "package p;\nclass Judged { CarMapperImpl g; Written w;\n    \
                 void m() { g.map(); g.nope(); w.gone(); } }\n"
            ),
            vec!["gone"]
        );
        assert_eq!(
            judge(
                &sources,
                "package p;\nclass Judged extends CarMapperImpl { Missing x; }\n"
            ),
            Vec::<String>::new()
        );
        assert_eq!(
            judge(
                &sources,
                "package p;\nclass Judged extends Written { Missing x; }\n"
            ),
            vec!["Missing"]
        );
    }

    /// A generated source's name is what the file means by it — here a
    /// same-package `String` — and the walk never read the type.
    #[test]
    fn a_receiver_type_a_build_may_generate_is_silent() {
        let text = "package p;\nclass Judged { String s; void m() { s.nope(); } }\n";
        assert_eq!(
            judge_with(&member_workspace(), text, &["String"], false),
            Vec::<String>::new()
        );
        assert_eq!(
            judge_with(&member_workspace(), text, &[], false),
            vec!["nope"]
        );
    }

    /// A workspace copy of a JDK type — GWT's super-source, a stub — is not
    /// the class the program runs against, and its list is not the JDK's.
    #[test]
    fn a_receiver_the_workspace_and_the_world_both_declare_is_silent() {
        let sources = indexed(
            "member-vendored",
            &[(
                "java/lang/String.java",
                "package java.lang;\npublic final class String {}\n",
            )],
        );
        assert_eq!(
            judge(
                &sources,
                "package p;\nclass Judged { String s; void m() { s.length(); } }\n"
            ),
            Vec::<String>::new()
        );
    }

    /// `import static a.Thing.Foo` brings in a member type called `Foo`,
    /// which shadows `p.Foo`.
    #[test]
    fn a_receiver_type_a_static_import_may_shadow_is_silent() {
        assert_eq!(
            judge_calls(
                "package p;\nimport static a.Thing.Foo;\nclass Judged { Foo f; void m() { f.nope(); } }\n"
            ),
            Vec::<String>::new()
        );
    }

    /// Inside `Judged extends Base`, `Mine extends Inner` extends
    /// `Base.Inner`, and a file-local receiver's supertypes are resolved
    /// without it.
    #[test]
    fn a_file_local_receivers_supertype_inheritance_may_shadow_is_silent() {
        assert_eq!(
            judge_calls(
                "package p;\nclass Judged extends Base {\n    \
                 static class Mine extends Inner { void m() { this.nope(); } } }\n"
            ),
            Vec::<String>::new()
        );
        assert_eq!(
            judge_calls(
                "package p;\nclass Judged {\n    \
                 static class Mine extends Inner { void m() { this.nope(); } } }\n"
            ),
            vec!["nope"],
            "without `Base`, `Inner` is the top-level one"
        );
    }

    /// The same, in another file: `Outer.Mine extends Inner` inside
    /// `Outer extends Base` means `Base.Inner`, and `Imp extends Foo` under
    /// `import static a.Thing.Foo` means `a.Thing.Foo` — and inside an enum,
    /// `java.lang.Enum`'s `EnumDesc` is in scope.
    #[test]
    fn a_workspace_receivers_supertype_inheritance_or_a_static_import_may_shadow_is_silent() {
        let sources = indexed(
            "member-shadowed-supertypes",
            &[
                (
                    "p/Base.java",
                    "package p;\npublic class Base { public static class Inner {} }\n",
                ),
                ("p/Inner.java", "package p;\npublic class Inner {}\n"),
                ("p/Foo.java", "package p;\npublic class Foo {}\n"),
                (
                    "p/Outer.java",
                    "package p;\npublic class Outer extends Base {\n    \
                     public static class Mine extends Inner {}\n    \
                     public static class Plain {} }\n",
                ),
                (
                    "p/Imp.java",
                    "package p;\nimport static a.Thing.Foo;\npublic class Imp extends Foo {}\n",
                ),
                (
                    "p/Day.java",
                    "package p;\npublic enum Day { ONE; public static class Desc extends Foo {} }\n",
                ),
            ],
        );
        assert_eq!(
            judge(
                &sources,
                "package p;\nclass Judged { Outer.Mine m; Imp i; Day.Desc d; Outer.Plain p;\n    \
                 void f() { m.nope(); i.nope(); d.nope(); p.none(); } }\n"
            ),
            vec!["none"]
        );
    }

    /// The file's own type, reached through a dependency's method, has the
    /// `private` members every other file's lists leave out.
    #[test]
    fn a_receiver_this_file_declares_is_silent_wherever_the_chain_found_it() {
        let text = "package p;\nimport lib.Factory;\nclass Judged { private void secret() {}\n    \
                    void m(Factory f) { f.make().secret(); f.make().nope(); } }\n";
        let sources = indexed("member-own", &[("p/Judged.java", text)]);
        let mut factory = fact("lib.Factory", DeclKind::Class, &["java.lang.Object"]);
        let mut make = method("make");
        make.declared = javelin_classpath::MemberType::Class("p.Judged".to_owned());
        factory.members = vec![make];
        factory.source = String::new();
        assert_eq!(
            run(
                &sources,
                text,
                Case {
                    jars: vec![factory],
                    ..Case::default()
                },
            ),
            Vec::<String>::new()
        );
    }

    /// The grammar reads no Unicode escape in an identifier, so a file
    /// spelling one has an error node and is not judged at all.
    #[test]
    fn a_name_spelled_with_a_unicode_escape_is_silent() {
        assert_eq!(
            judge_calls("package p;\nclass Judged { String s; void m() { s.\\u006eope(); } }\n"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_field_and_an_unqualified_call_nothing_declares_are_reported() {
        assert_eq!(
            judge_calls(
                "package p;\nclass Judged { String s; Foo f;\n    \
                 void m() { Object a = s.lenght; Object b = f.totl; helpr(); m(); } }\n"
            ),
            vec!["lenght", "totl", "helpr"]
        );
        let kinds: Vec<Kind> = found(
            &member_workspace(),
            "package p;\nclass Judged { String s; void m() { Object a = s.lenght; s.nope(); helpr(); } }\n",
            Case::default(),
        )
        .into_iter()
        .map(|found| found.kind)
        .collect();
        let string = || "java.lang.String".to_owned();
        assert_eq!(
            kinds,
            vec![
                Kind::Field { receiver: string() },
                Kind::Method { receiver: string() },
                Kind::Unqualified
            ]
        );
    }

    #[test]
    fn a_field_on_a_type_this_file_declares_is_silent() {
        assert_eq!(
            judge_calls("package p;\nclass Judged { void m() { Object a = this.nope; } }\n"),
            Vec::<String>::new()
        );
    }

    /// `Map.Entry` is a nested type no external closure lists, and
    /// `HashMap.Entry` is the same one inherited.
    #[test]
    fn a_field_through_a_type_name_is_silent_where_a_frame_declares_a_type_of_its_name() {
        assert_eq!(
            judge_calls(
                "package p;\nimport java.util.Map;\nimport java.util.HashMap;\n\
                 class Judged { void m() { Object a = Map.Entry; Object b = HashMap.Entry; \
                 Object c = HashMap.Nope; } }\n"
            ),
            vec!["Nope"]
        );
    }

    #[test]
    fn an_unqualified_call_a_static_import_may_bring_in_is_silent() {
        assert_eq!(
            judge_calls(
                "package p;\nimport static a.Thing.helpr;\n\
                 class Judged { void m() { helpr(); other(); } }\n"
            ),
            vec!["other"]
        );
    }

    /// Inside `Judged extends Base`, `Mine extends Inner` extends
    /// `Base.Inner`, not the `p.Inner` the resolver reads, so the list walked
    /// for `Mine` may be another type's.
    #[test]
    fn an_unqualified_call_in_a_type_whose_supertype_inheritance_may_shadow_is_silent() {
        assert_eq!(
            judge_calls(
                "package p;\nclass Judged extends Base {\n    \
                 class Mine extends Inner { void m() { helpr(); } } }\n"
            ),
            Vec::<String>::new()
        );
    }

    /// The types around an unqualified call are walked from this file, so
    /// their `private` members are in the list and the own-type rule a
    /// receiver gets does not apply — a default-package type's name is its
    /// fully-qualified one.
    #[test]
    fn an_unqualified_call_in_a_default_package_type_is_judged() {
        assert_eq!(
            run(
                &member_workspace(),
                "class Judged { private void mine() {} void m() { mine(); helpr(); } }\n",
                Case {
                    relative: "Judged.java",
                    ..Case::default()
                },
            ),
            vec!["helpr"]
        );
    }

    #[test]
    fn an_unqualified_call_in_a_type_reaching_the_jdk_is_silent_below_the_jdks_own_level() {
        let at = |text, level| {
            run(
                &member_workspace(),
                text,
                Case {
                    level,
                    ..Case::default()
                },
            )
        };
        let plain = "package p;\nclass Judged { void m() { helpr(); } }\n";
        let map = "package p;\nclass Judged extends java.util.HashMap { void m() { helpr(); } }\n";
        assert_eq!(at(plain, 17), vec!["helpr"], "only Object reached");
        assert_eq!(at(map, 21), vec!["helpr"], "the JDK's own level");
        assert_eq!(at(map, 17), Vec::<String>::new(), "an older level");
    }

    #[test]
    fn a_field_and_an_unqualified_call_are_published_under_their_own_codes() {
        let range = Range::new(
            javelin_document::Position::new(0, 0),
            javelin_document::Position::new(0, 4),
        );
        let published = Published {
            uri: "file:///p/Judged.java".to_owned(),
            document: None,
            found: vec![
                Unresolved {
                    name: "nope".to_owned(),
                    range,
                    kind: Kind::Field {
                        receiver: "java.lang.String".to_owned(),
                    },
                },
                Unresolved {
                    name: "gone".to_owned(),
                    range,
                    kind: Kind::Unqualified,
                },
            ],
        };
        let sent = notification(&published);
        let field = &sent.params["diagnostics"][0];
        assert_eq!(field["code"], "unresolved-field");
        assert_eq!(
            field["message"],
            "`java.lang.String` has no field `nope` that can be accessed here"
        );
        let call = &sent.params["diagnostics"][1];
        assert_eq!(call["code"], "unresolved-method");
        assert_eq!(
            call["message"],
            "no method `gone` that can be called here is in scope"
        );
    }

    #[test]
    fn an_unresolved_method_is_published_under_its_own_code() {
        let published = Published {
            uri: "file:///p/Judged.java".to_owned(),
            document: None,
            found: vec![Unresolved {
                name: "nope".to_owned(),
                range: Range::new(
                    javelin_document::Position::new(0, 0),
                    javelin_document::Position::new(0, 4),
                ),
                kind: Kind::Method {
                    receiver: "java.lang.String".to_owned(),
                },
            }],
        };
        let sent = notification(&published);
        let diagnostic = &sent.params["diagnostics"][0];
        assert_eq!(diagnostic["code"], "unresolved-method");
        assert_eq!(
            diagnostic["message"],
            "`java.lang.String` has no method `nope` that can be called here"
        );
    }

    #[test]
    fn an_import_of_a_type_nothing_holds_is_reported_by_its_name() {
        let sources = workspace();
        assert_eq!(
            judge(
                &sources,
                "package p;\nimport a.Nope;\nimport a.Thing;\nclass Judged { Nope n; Thing t; }\n"
            ),
            vec!["a.Nope"],
            "once, at the import, and not again where the import makes it resolve"
        );
    }
}
