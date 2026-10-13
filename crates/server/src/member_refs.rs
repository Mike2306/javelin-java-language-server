//! Resolving every member selection of a workspace file, on the workspace
//! worker's thread, for cross-file member references.
//!
//! A type reference can wait to be resolved until a query asks, because what
//! a type name means depends on its own file's package and imports alone. A
//! member selection cannot: what `g.greet()` means depends on `g`'s type,
//! which depends on declarations anywhere. So the worker runs the queries
//! definition runs — the same [`javelin_semantic::external_member_at`] — over
//! every selected member name, and records the ones that land on a workspace
//! member, keyed by that member's site — this file's own members included,
//! because the file is answered live only while it is the one open — and the
//! ones that land on a JAR member, or a JDK member of a type the workspace
//! extends, keyed by `library_key`.
//!
//! Beside them it records every fully-qualified name the resolution asked
//! about. Those are the file's dependencies: when a file declaring one of them
//! changes, the answers recorded here may have moved, and the worker resolves
//! this file again.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use javelin_core::{CancellationToken, Cancelled};
use javelin_document::{DocumentId, TextDocument};
use javelin_index::MemberRef;
use javelin_parser::{DefinitionKind, FileSyntax, ImportDecl, JavaParser, ScopeTree};
use javelin_semantic::{ExternalMembers, Externals, Primitive};

use crate::classpath::ClasspathExternals;

/// What one file's member selections resolved to, and what that depended on.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    pub refs: Vec<MemberRef>,
    pub deps: Vec<String>,
}

/// Resolve every selected member in `text`, the content of `path`.
///
/// `None` for text that does not parse at all; a file mid-edit still yields
/// whatever resolved.
pub fn resolve_file(
    path: &Path,
    text: String,
    parser: &mut JavaParser,
    externals: &ClasspathExternals,
    token: &CancellationToken,
) -> Result<Option<Resolved>, Cancelled> {
    token.check()?;
    let document = TextDocument::new(DocumentId::new(path.to_string_lossy()), 0, text);
    let Ok(tree) = parser.parse(&document) else {
        return Ok(None);
    };
    let syntax = FileSyntax::build(&document, &tree);
    let scopes = ScopeTree::build(&document, &tree);
    let files = javelin_semantic::Files::single(document.id().clone(), &syntax, &scopes);
    let recording = Recording::new(externals);
    let own = path.to_string_lossy();
    let mut refs = Vec::new();
    // And every bare name no scope binds: a field a supertype declares, or
    // one a static import brings in — a selection is not the only way to
    // name a workspace field. Disjoint from the selections, since no
    // expression name is selected.
    let mut spans = javelin_parser::member_name_spans(&document, &tree, token)?;
    spans.extend(javelin_parser::unbound_expression_names(
        &document, &tree, &scopes, token,
    )?);
    for span in spans {
        token.check()?;
        // This file's own field or method, by definition's local chain — the
        // one `handlers::definition_in` asks before any external question.
        //
        // ponytail: only selections and unbound names are visited, so a bare `count` naming
        // this file's own field is listed while this file is open and not
        // when references are asked from another one. The upgrade is visiting
        // every identifier the `ScopeTree` binds to a field, as the live half
        // does.
        let local = javelin_semantic::member_definition_at(
            &document,
            &tree,
            &files,
            &recording,
            span.start_byte,
        )
        .or_else(|| {
            javelin_semantic::call_definition_at(
                &document,
                &tree,
                &files,
                &recording,
                span.start_byte,
            )
        });
        if let Some(found) = local {
            if found.file == *document.id()
                && matches!(found.kind, DefinitionKind::Field | DefinitionKind::Method)
            {
                refs.push(MemberRef {
                    name: found.name,
                    start_byte: span.start_byte,
                    end_byte: span.end_byte,
                    range: span.range,
                    target_source: own.clone().into_owned(),
                    target_start: found.name_span.start_byte,
                });
            }
            continue;
        }
        let Some(found) = javelin_semantic::external_member_at(
            &document,
            &tree,
            &files,
            &recording,
            span.start_byte,
        ) else {
            continue;
        };
        // A workspace member's site carries a recorded range; a JDK or JAR
        // one does not, and is keyed by where its owner's facts list it.
        //
        // ponytail: a JDK member of a type nothing in the workspace extends —
        // every `toString` and `add` — would add a quarter to this table for
        // a question a reviewer rarely asks, so references on `String.length`
        // list the open file's uses only. The upgrade is dropping the
        // `records_library` gate; the handler reads the rows already.
        let (target_source, target_start) = match found.site {
            Some(site) if site.name_range.is_some() => (site.source, site.name_start),
            _ if externals.records_library(&found.owner) => {
                (library_key(&found.owner, &found.name, found.ordinal), 0)
            }
            _ => continue,
        };
        refs.push(MemberRef {
            name: found.name,
            start_byte: span.start_byte,
            end_byte: span.end_byte,
            range: span.range,
            target_source,
            target_start,
        });
    }
    // Every `new`, `this(…)`, `super(…)` and enum constant that selects a
    // workspace constructor, under the gates definition puts in front of one.
    //
    // ponytail: a name the survey saw generated is not refused here, since
    // the worker holds no survey, and neither is a file Lombok writes past
    // the model in; definition refuses both, so a `new` can be listed from
    // here and not jump. The upgrade is the generated names handed to
    // `ClasspathExternals` and `lombok_short` read off the index's rows.
    let creations = javelin_parser::creation_name_spans(&document, &tree, token)?;
    let explicit = javelin_parser::explicit_constructor_spans(&document, &tree, token)?;
    for span in creations.into_iter().chain(explicit) {
        token.check()?;
        let Some(found) = javelin_semantic::constructor_at(
            &document,
            &tree,
            &files,
            &recording,
            span.start_byte,
            true,
        ) else {
            continue;
        };
        if found
            .written
            .iter()
            .any(|name| crate::handlers::shadowed(name, &syntax, externals))
        {
            continue;
        }
        let (name, source, start) = match found.target {
            javelin_semantic::ConstructorTarget::Local(definition) => (
                definition.name,
                own.clone().into_owned(),
                definition.name_span.start_byte,
            ),
            javelin_semantic::ConstructorTarget::External(summary) => {
                // This file's own type read through the index lost the
                // `private` constructors its own code may call.
                let Some(site) = summary
                    .site
                    .filter(|site| site.name_range.is_some() && site.source != *own)
                else {
                    continue;
                };
                (summary.name, site.source, site.name_start)
            }
        };
        refs.push(MemberRef {
            name,
            start_byte: span.start_byte,
            end_byte: span.end_byte,
            range: span.range,
            target_source: source,
            target_start: start,
        });
    }
    Ok(Some(Resolved {
        refs,
        deps: recording.into_deps(),
    }))
}

/// The key a JDK or JAR member with no workspace site is recorded under, at
/// start `0`: its owner, its name and its place in the owner's facts. Never a
/// path, which is absolute wherever one is stored.
pub(crate) fn library_key(owner: &str, name: &str, ordinal: u32) -> String {
    format!("{owner}#{name}#{ordinal}")
}

/// An [`Externals`] that remembers every type it was asked about, and answers
/// each type's members once per file.
///
/// The memory is what makes the file's dependencies: a type whose members
/// were listed, or a name that resolved, is one whose change could move an
/// answer recorded here. The cache is what makes a file of five hundred calls
/// on one receiver type list that type's members once rather than five
/// hundred times.
pub(crate) struct Recording<'a> {
    inner: &'a ClasspathExternals,
    asked: RefCell<BTreeSet<String>>,
    members: RefCell<HashMap<String, Option<ExternalMembers>>>,
}

impl<'a> Recording<'a> {
    pub(crate) fn new(inner: &'a ClasspathExternals) -> Self {
        Self {
            inner,
            asked: RefCell::new(BTreeSet::new()),
            members: RefCell::new(HashMap::new()),
        }
    }

    fn into_deps(self) -> Vec<String> {
        self.asked.into_inner().into_iter().collect()
    }
}

impl Externals for Recording<'_> {
    /// The name as written and its first segment are dependencies whatever
    /// the answer: a type declared later under either can make a name that
    /// meant nothing mean something, or shadow what it meant.
    fn resolve(&self, name: &str, package: Option<&str>, imports: &[ImportDecl]) -> Option<String> {
        let found = self.inner.resolve(name, package, imports);
        let mut asked = self.asked.borrow_mut();
        asked.insert(name.to_owned());
        if let Some(first) = name.split('.').next() {
            asked.insert(first.to_owned());
        }
        if let Some(fqn) = &found {
            asked.insert(fqn.clone());
        }
        found
    }

    fn is_subtype(&self, from: &str, to: &str) -> Option<bool> {
        self.inner.is_subtype(from, to)
    }

    fn boxed(&self, primitive: Primitive) -> Option<String> {
        self.inner.boxed(primitive)
    }

    fn unboxed(&self, fqn: &str) -> Option<Primitive> {
        self.inner.unboxed(fqn)
    }

    fn members(&self, fqn: &str) -> Option<ExternalMembers> {
        self.asked.borrow_mut().insert(fqn.to_owned());
        if let Some(cached) = self.members.borrow().get(fqn) {
            return cached.clone();
        }
        // The closure's frames are dependencies too, the empty ones included:
        // an inherited member moves when a supertype between the receiver and
        // its owner gains an override, or when a missing one appears.
        let (found, frames) = self.inner.closure(fqn, true);
        self.asked.borrow_mut().extend(frames);
        self.members
            .borrow_mut()
            .insert(fqn.to_owned(), found.clone());
        found
    }

    fn constructors(&self, fqn: &str) -> Option<ExternalMembers> {
        // Whether a constructor is `certain`, and what its parameters mean,
        // turn on the type's closure and the names it writes, as a member's do.
        self.members(fqn);
        self.inner.constructors(fqn)
    }

    fn generated_members(
        &self,
        fqn: &str,
        file: &javelin_semantic::FileId,
    ) -> Vec<javelin_semantic::ExternalMember> {
        self.asked.borrow_mut().insert(fqn.to_owned());
        self.inner.generated_members(fqn, file)
    }

    fn may_shadow_type(&self, name: &str, syntax: &FileSyntax) -> bool {
        self.inner.may_shadow_type(name, syntax)
    }
}

#[cfg(test)]
fn method_facts(name: &str, parameters: &[&str]) -> javelin_classpath::MemberFacts {
    javelin_classpath::MemberFacts {
        type_parameters: Vec::new(),
        doc_span: None,
        parameter_names: Vec::new(),
        name: name.to_owned(),
        kind: javelin_parser::DeclKind::Method,
        is_static: false,
        is_abstract: false,
        is_public: true,
        declared: javelin_classpath::MemberType::Void,
        parameters: parameters
            .iter()
            .map(|fqn| javelin_classpath::MemberType::Class((*fqn).to_owned()))
            .collect(),
        variadic: false,
        name_start: 0,
        name_end: 0,
    }
}

/// A JDK of `Object`, `String` with `length()` and `InputStream` with
/// `read()`, and a JAR holding `org.lib.Repo` with `find()` and
/// `save(String)`.
#[cfg(test)]
pub(crate) fn library_world() -> std::sync::Arc<javelin_classpath::TypeWorld> {
    let fact = |fqn: &str, members| javelin_classpath::TypeFacts {
        members,
        ..crate::classpath::fact(fqn)
    };
    let jdk = javelin_classpath::TypeWorld::new(vec![
        fact("java.lang.Object", Vec::new()),
        fact("java.lang.String", vec![method_facts("length", &[])]),
        fact("java.io.InputStream", vec![method_facts("read", &[])]),
    ]);
    std::sync::Arc::new(jdk.extended(vec![fact(
        "org.lib.Repo",
        vec![
            method_facts("find", &[]),
            method_facts("save", &["java.lang.String"]),
        ],
    )]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classpath::Classpath;
    use crate::workspace::testing::indexed;
    use std::sync::Arc;

    const GREETER: &str = "package com.example;\npublic class Greeter {\n    \
                           void greet() {}\n    void pick(Object o) {}\n    void pick(String s) {}\n}\n";

    fn resolved(sources: &crate::workspace::testing::Sources, text: &str) -> Resolved {
        let externals = ClasspathExternals::new(
            Classpath::hand_built(&["java.lang.Object", "java.lang.String"]).world,
            Arc::clone(&sources.index),
        );
        let mut parser = JavaParser::new().expect("grammar");
        resolve_file(
            &sources.path("com/example/Caller.java"),
            text.to_owned(),
            &mut parser,
            &externals,
            &CancellationToken::new(),
        )
        .expect("not cancelled")
        .expect("parses")
    }

    #[test]
    fn a_call_to_a_member_another_file_declares_is_recorded_against_its_site() {
        let sources = indexed(
            "member-refs-cross",
            &[("com/example/Greeter.java", GREETER)],
        );
        let found = resolved(
            &sources,
            "package com.example;\nclass Caller { void m(Greeter g) { g.greet(); } }\n",
        );
        assert_eq!(found.refs.len(), 1, "{found:?}");
        let call = &found.refs[0];
        assert_eq!(call.name, "greet");
        assert_eq!(
            Path::new(&call.target_source),
            sources.path("com/example/Greeter.java")
        );
        assert_eq!(
            &GREETER[call.target_start as usize..call.target_start as usize + 5],
            "greet"
        );
        assert_eq!(call.range.start, javelin_document::Position::new(1, 37));
        assert!(
            found.deps.contains(&"com.example.Greeter".to_owned()),
            "{:?}",
            found.deps
        );
    }

    #[test]
    fn a_bare_inherited_field_is_recorded_against_the_supertypes_field() {
        const BASE: &str = "package com.example;\npublic class Base {\n    \
                            protected int count;\n    private int secret;\n}\n";
        let sources = indexed("member-refs-bare", &[("com/example/Base.java", BASE)]);
        let found = resolved(
            &sources,
            "package com.example;\nclass Caller extends Base { \
             int m(int other) { return count + secret + other; } }\n",
        );
        assert_eq!(found.refs.len(), 1, "only `count`, a member: {found:?}");
        let bare = &found.refs[0];
        assert_eq!(bare.name, "count");
        assert_eq!(
            Path::new(&bare.target_source),
            sources.path("com/example/Base.java")
        );
        assert_eq!(
            bare.target_start as usize,
            BASE.find("count").expect("the field")
        );
    }

    #[test]
    fn an_explicit_constructor_call_is_recorded_against_the_constructor_it_selects() {
        const BOX: &str = "package com.example;\npublic class Box {\n    \
                           public Box() {}\n    public Box(int a, int b) {}\n}\n";
        let sources = indexed("member-refs-super", &[("com/example/Box.java", BOX)]);
        let found = resolved(
            &sources,
            "package com.example;\nclass Caller extends Box { Caller() { super(1, 2); } }\n",
        );
        let at = BOX.find("Box(int").expect("the two-argument constructor");
        let keys: Vec<(&str, usize)> = found
            .refs
            .iter()
            .map(|it| (it.name.as_str(), it.target_start as usize))
            .collect();
        assert_eq!(keys, [("Box", at)], "{found:?}");
        // An enum's constants are its constructor calls, recorded in its file.
        let day =
            "package com.example;\nenum Caller { MON(1), TUE; Caller(int n) {} Caller() {} }\n";
        let found = resolved(&sources, day);
        let starts: Vec<usize> = found
            .refs
            .iter()
            .map(|it| it.target_start as usize)
            .collect();
        assert_eq!(
            starts,
            [
                day.find("Caller(int").expect("one"),
                day.find("Caller() {}").expect("none")
            ],
            "{found:?}"
        );
    }

    #[test]
    fn a_new_is_recorded_against_the_constructor_it_selects() {
        const BOX: &str = "package com.example;\npublic class Box {\n    \
                           public Box() {}\n    public Box(int a, int b) {}\n}\n";
        let sources = indexed("member-refs-new", &[("com/example/Box.java", BOX)]);
        let found = resolved(
            &sources,
            "package com.example;\nclass Caller { Object m() { return new Box(1, 2); } }\n",
        );
        assert_eq!(found.refs.len(), 1, "{found:?}");
        let new = &found.refs[0];
        assert_eq!(new.name, "Box");
        let at = BOX.find("Box(int").expect("the two-argument constructor");
        assert_eq!(new.target_start as usize, at);
        assert_eq!(new.range.start, javelin_document::Position::new(1, 39));
        // A type nothing in the workspace declares records nothing.
        let none = resolved(
            &sources,
            "package com.example;\nclass Caller { Object m() { return new Missing(1); } }\n",
        );
        assert!(none.refs.is_empty(), "{none:?}");
        // The file's own type read through the index lost its `private`
        // constructors, which Java picks here.
        let own = "package com.example;\npublic class Caller {\n    private Caller(int n) {}\n    \
                   public Caller(Object o) {}\n    \
                   static Caller of() { return new com.example.Caller(1); }\n}\n";
        let sources = indexed("member-refs-new-own", &[("com/example/Caller.java", own)]);
        let found = resolved(&sources, own);
        assert!(found.refs.is_empty(), "{found:?}");
        // What a constructor's parameters mean, and whether its type's
        // closure is whole, are dependencies of the `new` that selects it.
        let sources = indexed(
            "member-refs-new-deps",
            &[
                (
                    "com/example/Base.java",
                    "package com.example;\npublic class Base {}\n",
                ),
                (
                    "com/example/Box.java",
                    "package com.example;\npublic class Box extends Base {\n    \
                     public Box(Item item, int n) {}\n}\n",
                ),
            ],
        );
        let found = resolved(
            &sources,
            "package com.example;\nclass Caller { Object m() { return new Box(null, 1); } }\n",
        );
        for name in ["Item", "com.example.Base"] {
            assert!(
                found.deps.contains(&name.to_owned()),
                "{name}: {:?}",
                found.deps
            );
        }
    }

    #[test]
    fn an_ambiguous_call_is_recorded_against_neither_candidate() {
        let sources = indexed("member-refs-skip", &[("com/example/Greeter.java", GREETER)]);
        let found = resolved(
            &sources,
            "package com.example;\nclass Caller {\n  void m(Greeter g, Unknown u) { g.pick(u); }\n}\n",
        );
        assert!(found.refs.is_empty(), "{:?}", found.refs);
    }

    /// `@Column(name = …)`'s key is a use of `Column.name()` — and of
    /// nothing where a static import may mean another `Column`.
    #[test]
    fn an_annotation_key_is_recorded_against_its_element_unless_shadowed() {
        let column = "package com.example;\npublic @interface Column { String name(); }\n";
        let sources = indexed(
            "member-refs-annotation",
            &[("com/example/Column.java", column)],
        );
        let found = resolved(
            &sources,
            "package com.example;\nclass Caller { @Column(name = \"n\") String name; }\n",
        );
        assert_eq!(found.refs.len(), 1, "{:?}", found.refs);
        assert_eq!(
            Path::new(&found.refs[0].target_source),
            sources.path("com/example/Column.java")
        );
        assert_eq!(
            found.refs[0].target_start as usize,
            column.find("name").expect("the element")
        );
        let shadowed = resolved(
            &sources,
            "package com.example;\nimport static q.Mapping.Column;\n\
             class Caller { @Column(name = \"n\") String name; }\n",
        );
        assert!(shadowed.refs.is_empty(), "{:?}", shadowed.refs);
    }

    /// Recorded rather than left to the live half, because the live half only
    /// runs when this file is the one open: asked from a third file, the
    /// declaring file's own calls come out of the index like any other's.
    #[test]
    fn a_call_to_the_files_own_member_is_recorded_under_its_own_path() {
        let sources = indexed("member-refs-own", &[("com/example/Greeter.java", GREETER)]);
        let text = "package com.example;\nclass Caller {\n  void own() {}\n  void m() { own(); this.own(); }\n}\n";
        let found = resolved(&sources, text);
        let own = text.find("own()").expect("the declaration") as u32;
        assert_eq!(found.refs.len(), 2, "{:?}", found.refs);
        for call in &found.refs {
            assert_eq!(
                Path::new(&call.target_source),
                sources.path("com/example/Caller.java")
            );
            assert_eq!(call.target_start, own);
        }
    }

    /// A name that means nothing yet is a dependency too: the file that
    /// declares it can arrive later, and so can one that shadows what it
    /// means now.
    #[test]
    fn a_written_name_is_a_dependency_whether_or_not_it_resolved() {
        let sources = indexed(
            "member-refs-unresolved",
            &[("com/example/Greeter.java", GREETER)],
        );
        let found = resolved(
            &sources,
            "package com.example;\nclass Caller { void m(Missing x, Greeter g) { x.go(); g.greet(); } }\n",
        );
        for name in ["Missing", "Greeter"] {
            assert!(
                found.deps.contains(&name.to_owned()),
                "{name}: {:?}",
                found.deps
            );
        }
    }

    /// A name written in another file the closure visited is a dependency
    /// too: `make`'s return type means nothing until `Greeter.java` exists,
    /// and the chain through it answers nothing until then.
    #[test]
    fn a_name_written_in_a_visited_frame_is_a_dependency() {
        let sources = indexed(
            "member-refs-frame-names",
            &[(
                "com/example/Maker.java",
                "package com.example;\npublic class Maker {\n    Greeter make() { return null; }\n}\n",
            )],
        );
        let found = resolved(
            &sources,
            "package com.example;\nclass Caller { void m(Maker k) { k.make().greet(); } }\n",
        );
        assert!(
            found.deps.contains(&"Greeter".to_owned()),
            "{:?}",
            found.deps
        );
    }

    /// Every frame of the closure, not only the ones that declared something:
    /// an empty supertype between the receiver and the declaring one is where
    /// an override can appear.
    #[test]
    fn an_empty_supertype_in_a_member_closure_is_a_dependency() {
        let sources = indexed(
            "member-refs-frames",
            &[
                ("com/example/Greeter.java", GREETER),
                (
                    "com/example/Mid.java",
                    "package com.example;\npublic class Mid extends Greeter {}\n",
                ),
                (
                    "com/example/Loud.java",
                    "package com.example;\npublic class Loud extends Mid {}\n",
                ),
            ],
        );
        let found = resolved(
            &sources,
            "package com.example;\nclass Caller { void m(Loud l) { l.greet(); } }\n",
        );
        assert_eq!(found.refs.len(), 1, "{:?}", found.refs);
        assert!(
            found.deps.contains(&"com.example.Mid".to_owned()),
            "{:?}",
            found.deps
        );
    }

    fn resolved_against_libraries(
        sources: &crate::workspace::testing::Sources,
        text: &str,
    ) -> Resolved {
        let externals = ClasspathExternals::new(library_world(), Arc::clone(&sources.index));
        let mut parser = JavaParser::new().expect("grammar");
        resolve_file(
            &sources.path("com/example/Caller.java"),
            text.to_owned(),
            &mut parser,
            &externals,
            &CancellationToken::new(),
        )
        .expect("not cancelled")
        .expect("parses")
    }

    #[test]
    fn a_call_into_a_jar_member_is_recorded_under_its_owner_and_place() {
        let sources = indexed("member-refs-jar", &[]);
        let found = resolved_against_libraries(
            &sources,
            "package com.example;\nimport org.lib.Repo;\n\
             class Caller { void m(Repo r) { r.save(\"x\"); r.find(); } }\n",
        );
        let keys: Vec<(&str, &str, u32)> = found
            .refs
            .iter()
            .map(|it| (it.name.as_str(), it.target_source.as_str(), it.target_start))
            .collect();
        assert_eq!(
            keys,
            [
                ("save", library_key("org.lib.Repo", "save", 1).as_str(), 0),
                ("find", library_key("org.lib.Repo", "find", 0).as_str(), 0),
            ]
        );
    }

    #[test]
    fn a_call_into_a_type_two_jars_declare_differently_is_not_recorded() {
        let sources = indexed("member-refs-rivalled", &[]);
        let other = javelin_classpath::TypeFacts {
            members: vec![method_facts("other", &[]), method_facts("find", &[])],
            ..crate::classpath::fact("org.lib.Repo")
        };
        let world = Arc::new(library_world().extended(vec![other]));
        assert!(world.rivalled("org.lib.Repo"));
        let externals = ClasspathExternals::new(world, Arc::clone(&sources.index));
        let found = resolve_file(
            &sources.path("com/example/Caller.java"),
            "package com.example;\nimport org.lib.Repo;\n\
             class Caller { void m(Repo r) { r.find(); } }\n"
                .to_owned(),
            &mut JavaParser::new().expect("grammar"),
            &externals,
            &CancellationToken::new(),
        )
        .expect("not cancelled")
        .expect("parses");
        assert!(found.refs.is_empty(), "{:?}", found.refs);
    }

    #[test]
    fn a_call_into_a_jdk_member_is_recorded_only_where_the_workspace_extends_its_type() {
        let sources = indexed(
            "member-refs-jdk",
            &[(
                "com/example/Mine.java",
                "package com.example;\nimport java.io.InputStream;\n\
                 public abstract class Mine extends InputStream {}\n",
            )],
        );
        let found = resolved_against_libraries(
            &sources,
            "package com.example;\nimport java.io.InputStream;\n\
             class Caller { void m(InputStream in, String s) { in.read(); s.length(); } }\n",
        );
        let names: Vec<&str> = found.refs.iter().map(|it| it.name.as_str()).collect();
        assert_eq!(names, ["read"], "{found:?}");
        assert_eq!(
            found.refs[0].target_source,
            library_key("java.io.InputStream", "read", 0)
        );
    }
}
