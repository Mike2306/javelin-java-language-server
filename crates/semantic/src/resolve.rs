//! Turning a type as written into a type resolved against the file it was
//! written in.
//!
//! Which file that is comes from the caller: `Files::current` names it, and it
//! is deliberately not always the buffer the request named — a supertype walk
//! that follows a declaration into another file resolves *that* file's
//! `extends` clause against *that* file's package, imports and scopes. The
//! other files the set can reach are never searched for a matching name.
//!
//! The scope tree already separates Java's type and variable namespaces, so
//! deciding whether `T` is a type parameter or `Box` is a declared class is a
//! namespace question the tree answers. A name it does not bind goes on to
//! [`Externals`], and only what neither settles is [`Type::Unknown`]. What is
//! behind that door is the server's to decide and has grown: it was the JDK's
//! facts, and it is now the JDK's facts and the workspace's own sources asked
//! as one table — which is how a name here comes to mean a sibling file
//! without this module reaching into one.
//!
//! The order is the whole rule: the file is asked first, so a file-local
//! `class String` shadows `java.lang.String` and a type parameter shadows
//! both. Consulting the classpath first would let a JDK name win over a
//! declaration the reader is looking at. "The file" is two questions, not
//! one — what the scope tree binds here, and then what the file declares
//! anywhere; see [`resolve_type_ref`] for why the second is needed.

use javelin_parser::BindingKind;

use crate::externals::Externals;
use crate::files::{DeclSite, Files};
use crate::types::{Type, TypeRef, parse_type_ref};

/// Resolve a type reference as written at `offset`.
///
/// `offset` decides which declarations are in scope, so it must be where the
/// type was written rather than where the question was asked.
///
/// `pub(crate)` because it takes a `TypeRef`, which stays internal so that
/// `parse_type_ref` remains the only way one is built and its depth cap
/// therefore holds. [`resolve_written`] is the public door.
pub(crate) fn resolve_type_ref(
    reference: &TypeRef,
    files: &Files<'_>,
    externals: &dyn Externals,
    offset: u32,
) -> Type {
    let syntax = files.current_syntax();
    let scopes = files.current_scopes();
    match reference {
        TypeRef::Primitive(primitive) => Type::Primitive(*primitive),
        TypeRef::Void => Type::Void,
        TypeRef::Array(element) => Type::Array(Box::new(resolve_type_ref(
            element, files, externals, offset,
        ))),
        TypeRef::Wildcard => Type::Unknown("?".into()),
        TypeRef::Named { name, arguments } => {
            let Some(binding) = scopes.resolve_type(offset, name) else {
                // Unbound at this offset is not the same as undeclared. A
                // member type is bound `Visibility::WholeScope` inside its
                // *own* enclosing type's scope, so `class Base { class Inner
                // {} }` leaves `Inner` unbound at a use in `class Sub extends
                // Base { Inner i; }` — legal Java that resolves to nothing.
                // Asking the classpath there would answer a type the user is
                // looking at with someone else's fully-qualified name, and a
                // wrong answer is worse than the missing one this returns
                // instead. Both checks must hold for the classpath to answer,
                // and `declares_type_named` is asked second because it used to
                // be the expensive one; now that it is a hash lookup the order
                // no longer matters for cost, only the file's precedence does.
                let Some(fqn) = externals.resolve(
                    name,
                    syntax.package.as_ref().map(|package| package.name.as_str()),
                    &syntax.imports,
                ) else {
                    return Type::Unknown(written(reference));
                };
                // A dotted name resolves on its first segment, so that is what
                // is being shadowed: `Map.Entry` asks whether the file
                // declares `Map`.
                //
                // ponytail: over-refuses, deliberately in this direction. In
                // `class A { class String {} } class B { String s; }` Java
                // says `B`'s `String` is `java.lang.String`, because
                // `A.String` is not in scope there, and this answers nothing.
                // The upgrade is to search the use's enclosing type chain and
                // its file-local supertype closure instead of the whole file
                // — which is `members.rs`'s walk rather than a new index.
                let first = name.split('.').next().unwrap_or(name);
                if syntax.declares_type_named(first) {
                    return Type::Unknown(written(reference));
                }
                return Type::External {
                    name: name.clone(),
                    fqn,
                    arguments: resolve_arguments(arguments, files, externals, offset),
                };
            };
            match binding.kind {
                BindingKind::TypeParameter => Type::Variable(name.clone()),
                BindingKind::TypeDeclaration => {
                    let span = binding.name_span;
                    // A binding the syntax view does not know is not resolvable
                    // any further; the two are built from one tree, so this is
                    // a guard rather than an expected case.
                    if syntax.declaration_at(span).is_none() {
                        return Type::Unknown(written(reference));
                    }
                    Type::Declared {
                        name: name.clone(),
                        // The scope tree that bound the name is the current
                        // file's, so the declaration it named is too.
                        decl: DeclSite::new(files.current().clone(), span),
                        arguments: resolve_arguments(arguments, files, externals, offset),
                    }
                }
                _ => Type::Unknown(written(reference)),
            }
        }
    }
}

/// Resolve a type's arguments, each on its own: `List<Box>` in a file that
/// declares `Box` but not `List` still knows what its argument is.
fn resolve_arguments(
    arguments: &[TypeRef],
    files: &Files<'_>,
    externals: &dyn Externals,
    offset: u32,
) -> Vec<Type> {
    arguments
        .iter()
        .map(|argument| resolve_type_ref(argument, files, externals, offset))
        .collect()
}

/// Resolve a type written as source text: the common case, since every type in
/// `FileSyntax` is stored as the author wrote it.
pub fn resolve_written(
    text: &str,
    files: &Files<'_>,
    externals: &dyn Externals,
    offset: u32,
) -> Type {
    resolve_type_ref(&parse_type_ref(text), files, externals, offset)
}

/// Render a reference back to something close to what was written, for the
/// text an `Unknown` carries for when a hover renders a type.
fn written(reference: &TypeRef) -> String {
    match reference {
        // Not unreachable: `List<int[]>` is legal Java, and if `List` does not
        // resolve then the whole reference is rendered here, so a hover that
        // renders a type has it. An empty arm would silently drop what the
        // author wrote.
        TypeRef::Primitive(primitive) => primitive.keyword().to_owned(),
        TypeRef::Void => "void".into(),
        TypeRef::Wildcard => "?".into(),
        TypeRef::Array(element) => format!("{}[]", written(element)),
        TypeRef::Named { name, arguments } if arguments.is_empty() => name.clone(),
        TypeRef::Named { name, arguments } => {
            let inner: Vec<String> = arguments.iter().map(written).collect();
            format!("{name}<{}>", inner.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::externals::{ExternalMembers, NoExternals};
    use crate::files::FileId;
    use crate::types::Primitive;
    use javelin_document::{DocumentId, TextDocument};
    use javelin_parser::{FileSyntax, ImportDecl, JavaParser, ScopeTree};

    /// The one file every fixture below is, named the same way its document is
    /// so that a `Files` set and a `TextDocument` agree on who they are.
    const FILE: &str = "file:///A.java";

    /// A stand-in classpath: a list of fully-qualified names, resolved by
    /// their last segment. A simple name two of them share answers `None`,
    /// which is a real ambiguity rather than a missing entry — and the point
    /// is that nothing downstream can tell which of the two it got.
    ///
    /// Hand-written on purpose. Reading a `src.zip` or a `TypeWorld` here
    /// would test the classpath, and what is under test is the door.
    struct Classpath {
        names: &'static [&'static str],
        /// The compilation unit the caller must describe: its package, and how
        /// many imports it has. Asserted inside `resolve` rather than on the
        /// result, because a door that forwards the wrong package resolves a
        /// name against the wrong file and no assertion out here would show it.
        unit: (Option<&'static str>, usize),
    }

    /// A classpath asked about a file with no package and no imports, which is
    /// every fixture below but one.
    fn classpath(names: &'static [&'static str]) -> Classpath {
        Classpath {
            names,
            unit: (None, 0),
        }
    }

    impl Externals for Classpath {
        fn resolve(
            &self,
            name: &str,
            package: Option<&str>,
            imports: &[ImportDecl],
        ) -> Option<String> {
            assert_eq!(
                (package, imports.len()),
                self.unit,
                "the caller must forward the open file's own package and imports"
            );
            let mut hits = self.names.iter().filter(|fqn| last_segment(fqn) == name);
            let first = *hits.next()?;
            hits.next().is_none().then(|| first.to_owned())
        }

        // Nothing on the resolution path asks these four: they answer for a
        // type already resolved, and are exercised where their real
        // implementation lives.
        fn is_subtype(&self, _from: &str, _to: &str) -> Option<bool> {
            None
        }

        fn boxed(&self, _primitive: Primitive) -> Option<String> {
            None
        }

        fn unboxed(&self, _fqn: &str) -> Option<Primitive> {
            None
        }

        fn members(&self, _fqn: &str) -> Option<ExternalMembers> {
            None
        }
    }

    fn last_segment(fqn: &str) -> &str {
        fqn.rsplit('.').next().unwrap_or(fqn)
    }

    struct Fixture {
        syntax: FileSyntax,
        scopes: ScopeTree,
    }

    impl Fixture {
        fn files(&self) -> Files<'_> {
            Files::single(FileId::new(FILE), &self.syntax, &self.scopes)
        }
    }

    fn fixture(source: &str) -> Fixture {
        let document = TextDocument::new(DocumentId::new(FILE), 1, source.to_owned());
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        Fixture {
            syntax: FileSyntax::build(&document, &tree),
            scopes: ScopeTree::build(&document, &tree),
        }
    }

    /// Resolve against the open file alone, which is every test written
    /// before a classpath existed.
    fn resolve(source: &str, written: &str, at: u32) -> Type {
        resolve_with(source, written, at, &NoExternals)
    }

    fn resolve_with(source: &str, written: &str, at: u32, externals: &dyn Externals) -> Type {
        let fixture = fixture(source);
        resolve_type_ref(&parse_type_ref(written), &fixture.files(), externals, at)
    }

    #[test]
    fn a_type_declared_in_this_file_resolves_to_its_declaration() {
        let source = "class Box {} class User { Box b; }";
        let at = source.find("Box b").expect("the field") as u32;
        let resolved = resolve(source, "Box", at);
        let Type::Declared { name, .. } = &resolved else {
            panic!("expected a declared type, got {resolved:?}");
        };
        assert_eq!(name, "Box");
    }

    #[test]
    fn a_type_this_file_does_not_declare_is_unknown() {
        let source = "class User { String s; }";
        let at = source.find("String s").expect("the field") as u32;
        assert_eq!(
            resolve(source, "String", at),
            Type::Unknown("String".into())
        );
    }

    #[test]
    fn a_type_parameter_in_scope_resolves_to_a_variable() {
        let source = "class Box<T> { T value; }";
        let at = source.find("T value").expect("the field") as u32;
        assert_eq!(resolve(source, "T", at), Type::Variable("T".into()));
    }

    #[test]
    fn a_primitive_resolves_without_consulting_the_file() {
        assert_eq!(
            resolve("class A {}", "int", 0),
            Type::Primitive(Primitive::Int)
        );
        assert_eq!(resolve("class A {}", "void", 0), Type::Void);
    }

    #[test]
    fn an_array_resolves_its_element_type() {
        let source = "class Box {} class User { Box[] all; }";
        let at = source.find("Box[] all").expect("the field") as u32;
        let Type::Array(element) = resolve(source, "Box[]", at) else {
            panic!("expected an array");
        };
        assert!(matches!(*element, Type::Declared { .. }));
    }

    #[test]
    fn type_arguments_resolve_independently_of_the_type_itself() {
        let source = "class Box {} class User { List<Box> all; }";
        let at = source.find("List<Box>").expect("the field") as u32;
        // `List` is not in the file, but `Box` is, and the argument is still
        // worth resolving: a later phase resolves the container.
        assert_eq!(
            resolve(source, "List<Box>", at),
            Type::Unknown("List<Box>".into())
        );
    }

    #[test]
    fn a_wildcard_is_unknown() {
        assert_eq!(resolve("class A {}", "?", 0), Type::Unknown("?".into()));
    }

    #[test]
    fn a_dotted_name_is_looked_up_whole_and_therefore_unknown() {
        let source = "class Outer { class Inner {} } class User { Outer.Inner i; }";
        let at = source.find("Outer.Inner i").expect("the field") as u32;
        // The leftmost segment resolves, the rest does not: qualified nested
        // lookup is not in this sub-project.
        assert_eq!(
            resolve(source, "Outer.Inner", at),
            Type::Unknown("Outer.Inner".into())
        );
    }

    #[test]
    fn a_primitive_nested_in_an_unresolvable_container_keeps_its_keyword() {
        // `List<int[]>` is legal Java. If the text an unresolvable type carries
        // drops the `int`, a hover shows the user `List<[]>`.
        let source = "class User { List<int[]> all; }";
        let at = source.find("List<int[]>").expect("the field") as u32;
        assert_eq!(
            resolve(source, "List<int[]>", at),
            Type::Unknown("List<int[]>".into())
        );
    }

    #[test]
    fn the_public_entry_point_parses_and_resolves_in_one_step() {
        let source = "class Box {} class User { Box b; }";
        let fixture = fixture(source);
        let at = source.find("Box b").expect("the field") as u32;
        let resolved = resolve_written("Box", &fixture.files(), &NoExternals, at);
        assert!(matches!(resolved, Type::Declared { .. }));
    }

    #[test]
    fn a_name_the_file_declares_shadows_the_same_name_on_the_classpath() {
        let source = "class String {} class User { String s; }";
        let at = source.find("String s").expect("the field") as u32;
        let resolved = resolve_with(source, "String", at, &classpath(&["java.lang.String"]));
        let Type::Declared { name, .. } = &resolved else {
            panic!("expected the file's own String, got {resolved:?}");
        };
        assert_eq!(name, "String");
    }

    #[test]
    fn a_type_parameter_shadows_the_classpath() {
        // Legal Java, and the tightest form of the rule: the parameter is in
        // scope, so the classpath is never asked.
        let source = "class Box<String> { String value; }";
        let at = source.find("String value").expect("the field") as u32;
        assert_eq!(
            resolve_with(source, "String", at, &classpath(&["java.lang.String"])),
            Type::Variable("String".into())
        );
    }

    #[test]
    fn a_name_only_the_classpath_knows_resolves_to_an_external_type() {
        let source = "package com.example; import java.util.List; \
                      class Box {} class User { String s; List<Box> all; }";
        let at = source.find("String s").expect("the field") as u32;
        let classpath = Classpath {
            names: &["java.lang.String", "java.util.List"],
            unit: (Some("com.example"), 1),
        };
        assert_eq!(
            resolve_with(source, "String", at, &classpath),
            Type::External {
                name: "String".into(),
                fqn: "java.lang.String".into(),
                arguments: Vec::new(),
            }
        );
        // An external type's arguments resolve on their own terms, the same as
        // a declared one's: the container comes from the classpath and the
        // argument from the file.
        let at = source.find("List<Box>").expect("the field") as u32;
        let Type::External { fqn, arguments, .. } =
            resolve_with(source, "List<Box>", at, &classpath)
        else {
            panic!("expected an external List");
        };
        assert_eq!(fqn, "java.util.List");
        assert!(matches!(arguments.as_slice(), [Type::Declared { .. }]));
    }

    #[test]
    fn an_ambiguous_classpath_name_stays_unknown() {
        // Two reachable types of one simple name. `Externals::resolve` cannot
        // report which kind of nothing this is, so `Unknown` is the answer for
        // an ambiguity exactly as it is for a name no world holds.
        let source = "class User { List l; }";
        let at = source.find("List l").expect("the field") as u32;
        assert_eq!(
            resolve_with(
                source,
                "List",
                at,
                &classpath(&["java.util.List", "java.awt.List"])
            ),
            Type::Unknown("List".into())
        );
    }

    #[test]
    fn a_name_nothing_knows_stays_unknown() {
        let source = "class User { Whatever w; }";
        let at = source.find("Whatever w").expect("the field") as u32;
        assert_eq!(
            resolve_with(source, "Whatever", at, &classpath(&["java.lang.String"])),
            Type::Unknown("Whatever".into())
        );
    }

    #[test]
    fn no_externals_reproduces_phase_twos_answers_exactly() {
        let source = "class User { String s; }";
        let at = source.find("String s").expect("the field") as u32;
        assert_eq!(
            resolve(source, "String", at),
            Type::Unknown("String".into())
        );
        // And it answers that because it answers nothing at all, not because
        // it holds a `java.lang` table it consults selectively.
        assert_eq!(
            NoExternals.resolve("String", Some("com.example"), &[]),
            None
        );
        assert_eq!(
            NoExternals.is_subtype("java.lang.Integer", "java.lang.Object"),
            None
        );
        assert_eq!(NoExternals.boxed(Primitive::Int), None);
        assert_eq!(NoExternals.unboxed("java.lang.Integer"), None);
    }

    #[test]
    fn a_type_the_file_declares_is_never_taken_from_the_classpath() {
        // Legal Java the scope tree does not bind: a member type is visible in
        // its own enclosing type's scope, not in a subclass's. Unbound here is
        // not undeclared, and only undeclared lets the classpath answer.
        let source = "class Base { class Inner {} } class Sub extends Base { Inner i; }";
        let at = source.find("Inner i").expect("the field") as u32;
        assert!(
            fixture(source).scopes.resolve_type(at, "Inner").is_none(),
            "the premise: nothing binds Inner at this offset"
        );
        assert_eq!(
            resolve_with(source, "Inner", at, &classpath(&["com.other.Inner"])),
            Type::Unknown("Inner".into())
        );
    }
}
