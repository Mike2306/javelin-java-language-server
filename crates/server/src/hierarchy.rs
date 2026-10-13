//! The inverse edges: which workspace types extend or implement a type, and
//! which of their members override a method.
//!
//! Answered from the published index on the request loop. A subtype is found
//! by name first — [`WorkspaceIndex::types_writing`] — and kept only when what
//! it wrote resolves to the type, through the same
//! [`ClasspathExternals::source_supertypes`] the member closure walks upward.
//! So the two directions cannot disagree about an edge, and a written name
//! that resolves to nothing or to two types is no edge in either.

use std::collections::{HashSet, VecDeque};

use javelin_core::{CancellationToken, Cancelled};
use javelin_index::{WorkspaceIndex, WorkspaceType};
use javelin_semantic::{ExternalMember, Externals};

use crate::classpath::ClasspathExternals;

/// How many types a transitive walk visits before it stops: a generated
/// hierarchy can be wide, and the walk is on the request loop.
const MAX_TYPES: usize = 10_000;

/// The workspace types that name `fqn` among their direct supertypes.
pub fn direct_subtypes<'a>(
    externals: &ClasspathExternals,
    index: &'a WorkspaceIndex,
    fqn: &str,
) -> Vec<&'a WorkspaceType> {
    let simple = fqn.rsplit('.').next().unwrap_or(fqn);
    index
        .types_writing(simple)
        .filter(|candidate| {
            candidate.fqn != fqn
                && externals
                    .source_supertypes(candidate)
                    .0
                    .iter()
                    .any(|supertype| supertype == fqn)
        })
        .collect()
}

/// Every workspace type below `fqn`, nearest first, each declaration once —
/// two files declaring one subtype's name are both listed, as
/// [`direct_subtypes`] lists them, and walked below once.
pub fn all_subtypes<'a>(
    externals: &ClasspathExternals,
    index: &'a WorkspaceIndex,
    fqn: &str,
) -> Vec<&'a WorkspaceType> {
    let mut walked: HashSet<String> = HashSet::from([fqn.to_owned()]);
    let mut listed: HashSet<(&std::path::Path, u32)> = HashSet::new();
    let mut found = Vec::new();
    let mut queue = VecDeque::from([fqn.to_owned()]);
    while let Some(next) = queue.pop_front() {
        for subtype in direct_subtypes(externals, index, &next) {
            if found.len() >= MAX_TYPES {
                return found;
            }
            if listed.insert((subtype.file.as_path(), subtype.name_start)) {
                found.push(subtype);
            }
            if walked.insert(subtype.fqn.clone()) {
                queue.push_back(subtype.fqn.clone());
            }
        }
    }
    found
}

/// The direct supertypes of `ty` that the workspace declares, exactly once
/// each. One it declares nowhere is `external_supertypes`'.
pub fn direct_supertypes<'a>(
    externals: &ClasspathExternals,
    index: &'a WorkspaceIndex,
    ty: &WorkspaceType,
) -> Vec<&'a WorkspaceType> {
    externals
        .source_supertypes(ty)
        .0
        .iter()
        .filter(|fqn| **fqn != ty.fqn)
        .filter_map(|fqn| {
            let mut declared = index.with_fqn(fqn);
            match (declared.next(), declared.next()) {
                (Some(one), None) => Some(one),
                _ => None,
            }
        })
        .collect()
}

/// The direct supertypes of `ty` the workspace declares nowhere — a JDK or
/// a JAR type — where what `ty` wrote certainly means them, and no segment
/// of it is a name a generator writes and the index never read.
///
/// Stricter than [`direct_supertypes`], which reads the tiers as the member
/// closure does: a JDK type shown above a workspace one is a claim nothing
/// else in the hierarchy makes, and `Error` is `java.lang.Error` only where
/// no inherited member type may mean it.
///
/// ponytail: the supertype nobody writes — `Object` for a class, `Enum` and
/// `Record` — is not listed; the upgrade is the kind rule `closure_frames`
/// already applies, plus telling a class's `implements` from its `extends`.
pub(crate) fn external_supertypes(
    externals: &ClasspathExternals,
    index: &WorkspaceIndex,
    ty: &WorkspaceType,
    generated: &std::collections::BTreeSet<String>,
    modules: Option<&std::sync::Arc<crate::modules::Modules>>,
) -> Vec<String> {
    certain(externals, ty, generated, modules)
        .into_iter()
        .filter(|fqn| *fqn != ty.fqn && index.with_fqn(fqn).next().is_none())
        .collect()
}

/// The workspace types whose certain supertypes name `fqn`, a type the
/// workspace declares nowhere — so the two directions agree about an edge.
pub(crate) fn external_subtypes<'a>(
    externals: &ClasspathExternals,
    index: &'a WorkspaceIndex,
    fqn: &str,
    generated: &std::collections::BTreeSet<String>,
    modules: Option<&std::sync::Arc<crate::modules::Modules>>,
) -> Vec<&'a WorkspaceType> {
    // Every type writing the name, not `direct_subtypes`' — its tiers read
    // over every module's JARs, and may mean another module's type.
    let simple = fqn.rsplit('.').next().unwrap_or(fqn);
    index
        .types_writing(simple)
        .filter(|candidate| {
            certain(externals, candidate, generated, modules)
                .iter()
                .any(|s| s == fqn)
        })
        .collect()
}

/// What `ty` certainly extends, read against its own module's JARs: a
/// same-package type in a JAR only another module depends on does not
/// outrank `java.lang` for it. A file no module claims is read for nothing,
/// as the type diagnostic judges it for nothing.
fn certain(
    externals: &ClasspathExternals,
    ty: &WorkspaceType,
    generated: &std::collections::BTreeSet<String>,
    modules: Option<&std::sync::Arc<crate::modules::Modules>>,
) -> Vec<String> {
    let scoped;
    let externals = match modules.map(|modules| modules.scope_of(&ty.file)) {
        None => externals,
        Some(None) => return Vec::new(),
        Some(Some(scope)) => {
            scoped = externals.scoped(scope);
            &scoped
        }
    };
    externals
        .certain_supertypes(ty)
        .into_iter()
        .filter(|(written, _)| {
            !written
                .split('.')
                .any(|segment| generated.contains(segment))
        })
        .map(|(_, fqn)| fqn)
        .collect()
}

/// The member of `owner` whose name starts at `start` in its source, with its
/// types resolved — the method whose overrides a caller wants.
pub fn member(externals: &ClasspathExternals, owner: &str, start: u32) -> Option<ExternalMember> {
    externals
        .members(owner)?
        .members
        .into_iter()
        .find(|member| {
            member.owner == owner
                && member
                    .site
                    .as_ref()
                    .is_some_and(|site| site.name_start == start)
        })
        .map(std::sync::Arc::unwrap_or_clone)
}

/// Every override of `method` — declared by `owner` — in the workspace types
/// below it: in each, its own non-static method of the same name, arity and
/// variadicity whose parameters match. Equal, and anything where the
/// overridden one *wrote* one of its type parameters — a type variable the
/// subtype may have pinned. An unresolved parameter that is not a type
/// variable — an array, a JDK type before the JDK arrives — matches only its
/// own spelling. Two matches in one type is ambiguity, and that type
/// contributes neither.
///
/// ponytail: two unresolved parameters of one spelling match, so `Request`
/// imported from different packages on each side — neither loaded — lists a
/// lone same-arity method as an override. Refusing instead would lose every
/// JDK-typed override on a machine with no JDK. The upgrade is resolving both
/// spellings to where they were imported from before comparing.
///
/// ponytail: no access modifier is indexed, so a package-private method is
/// listed as overridden from another package, where Java does not inherit it.
/// `override_line` refuses that shape; the upgrade is indexing each member's
/// access.
///
/// ponytail: the type-variable rule matches by name and arity alone, so a
/// subtype's same-arity overload of a generic method is listed as an
/// override when it is the only one. The upgrade is substituting the
/// subtype's type arguments into the parameters before comparing.
pub fn overrides<'a>(
    externals: &ClasspathExternals,
    index: &'a WorkspaceIndex,
    owner: &str,
    method: &ExternalMember,
) -> Vec<(&'a WorkspaceType, ExternalMember)> {
    if method.is_static || method.kind != javelin_classpath::DeclKind::Method {
        return Vec::new();
    }
    // Which of the overridden method's parameters it wrote as a type
    // variable, from how the index recorded it as written.
    let Some(written) = method
        .site
        .as_ref()
        .and_then(|site| index.member_at(std::path::Path::new(&site.source), site.name_start))
    else {
        return Vec::new();
    };
    let variables: Vec<bool> = written
        .parameters
        .iter()
        .map(|parameter| {
            javelin_classpath::names_a_type_parameter(parameter, &written.type_parameters)
        })
        .collect();
    let mut found = Vec::new();
    for subtype in all_subtypes(externals, index, owner) {
        let Some(members) = externals.own_members(&subtype.fqn) else {
            continue;
        };
        let mut matching = members.into_iter().filter(|candidate| {
            candidate.kind == javelin_classpath::DeclKind::Method
                && !candidate.is_static
                && candidate.name == method.name
                && candidate.variadic == method.variadic
                && candidate.parameters.len() == method.parameters.len()
                && candidate
                    .parameters
                    .iter()
                    .zip(&method.parameters)
                    .zip(&variables)
                    .all(|((mine, theirs), variable)| *variable || mine == theirs)
        });
        if let (Some(one), None) = (matching.next(), matching.next()) {
            found.push((subtype, one));
        }
    }
    found
}

/// The method at `start` in `source`, named `name`, every override of it
/// below, and every method above that it overrides — each by its site, the
/// method's own first.
///
/// The edge is [`overrides`]' own match — name, arity, variadicity,
/// parameters — read in both directions, but *strict*: a reference listed
/// for the wrong method is a wrong answer where an implementation listed for
/// one is a lead, so two rules that `overrides` relaxes hold here. A
/// parameter matches only an equal one — `overrides` lets a type variable
/// match anything, which also matches a generic method's own variable and an
/// overload — and a class's method is no override from another package,
/// because the index stores no access modifier and a package-private method
/// is not inherited there. An interface's methods are public, so that rule
/// costs nothing for them. Siblings — the other overrides of what the target
/// overrides — are not on the line.
///
/// ponytail: so an override that pins a supertype's type variable
/// (`Disk implements Store<String>`'s `put(String)`) and one of a public
/// class method from another package are off the line. The upgrade is
/// substituting the subtype's arguments before comparing, and indexing each
/// member's access.
///
/// ponytail: a JDK or JAR method above has no workspace site and is left
/// out, so overriding `Runnable.run` adds nothing; calls into one are not
/// recorded anyway.
pub fn override_line(
    externals: &ClasspathExternals,
    index: &WorkspaceIndex,
    (source, start, name): (&str, u32, &str),
    token: &CancellationToken,
) -> Result<Vec<(String, u32)>, Cancelled> {
    let mut line = vec![(source.to_owned(), start)];
    // The buffer's offset against the index's row: a quiet period behind an
    // edit, another method can stand at it.
    let Some(owner) = index
        .member_at(std::path::Path::new(source), start)
        .filter(|indexed| indexed.name == name)
        .map(|indexed| indexed.owner.clone())
    else {
        return Ok(line);
    };
    let Some(method) = member(externals, &owner, start) else {
        return Ok(line);
    };
    if method.kind != javelin_classpath::DeclKind::Method || method.is_static {
        return Ok(line);
    }
    let (Some(owner_type), Some(own)) = (one_type(index, &owner), externals.own_members(&owner))
    else {
        return Ok(line);
    };
    // Below: each subtype's own matching method.
    for subtype in all_subtypes(externals, index, &owner) {
        token.check()?;
        if !inherits_across(index, owner_type, subtype) {
            continue;
        }
        let Some(members) = externals.own_members(&subtype.fqn) else {
            continue;
        };
        if let Some(site) = single(&members, &method).and_then(|found| written_site(index, found)) {
            line.push(site);
        }
    }
    // Above: each supertype's method of which the target is the owner's one
    // match — the same edge, from the other end, against members read once
    // per type.
    let target = Some((source.to_owned(), start));
    let mut seen: HashSet<String> = HashSet::from([owner.clone()]);
    let mut queue: VecDeque<&WorkspaceType> = VecDeque::from([owner_type]);
    while let Some(next) = queue.pop_front() {
        for above in direct_supertypes(externals, index, next) {
            token.check()?;
            if seen.len() >= MAX_TYPES || !seen.insert(above.fqn.clone()) {
                continue;
            }
            queue.push_back(above);
            if !inherits_across(index, above, owner_type) {
                continue;
            }
            let Some(candidates) = externals.own_members(&above.fqn) else {
                continue;
            };
            for overridden in candidates.iter().filter(|candidate| {
                candidate.name == method.name
                    && candidate.kind == javelin_classpath::DeclKind::Method
                    && !candidate.is_static
            }) {
                if single(&own, overridden).and_then(site_of) == target
                    && let Some(site) = written_site(index, overridden)
                {
                    line.push(site);
                }
            }
        }
    }
    Ok(line)
}

/// `found`'s site, when a written declaration of that name stands there. A
/// member Lombok generates is sited at its field, whose uses are not uses of
/// the method it overrides — so it is off the line, though implementation
/// still lists it.
///
/// ponytail: so references on `Named.getName` miss the calls through a
/// Lombok getter that implements it. The upgrade is a site of its own for a
/// generated member, which the index has no offset to give.
fn written_site(index: &WorkspaceIndex, found: &ExternalMember) -> Option<(String, u32)> {
    let site = site_of(found)?;
    index
        .member_at(std::path::Path::new(&site.0), site.1)
        .is_some_and(|written| written.name == found.name)
        .then_some(site)
}

fn site_of(found: &ExternalMember) -> Option<(String, u32)> {
    found
        .site
        .as_ref()
        .map(|site| (site.source.clone(), site.name_start))
}

/// The one member of `members` that overrides `method` by the strict rule —
/// equal parameters, never a type variable standing in — or none for none or
/// two.
fn single<'m>(
    members: &'m [ExternalMember],
    method: &ExternalMember,
) -> Option<&'m ExternalMember> {
    let mut matching = members.iter().filter(|candidate| {
        candidate.kind == javelin_classpath::DeclKind::Method
            && !candidate.is_static
            && candidate.name == method.name
            && candidate.variadic == method.variadic
            && candidate.parameters == method.parameters
    });
    match (matching.next(), matching.next()) {
        (Some(one), None) => Some(one),
        _ => None,
    }
}

/// Whether a method `above` declares can be overridden in `below` for all
/// this can tell: an interface's always, a class's only within its package.
fn inherits_across(index: &WorkspaceIndex, above: &WorkspaceType, below: &WorkspaceType) -> bool {
    if matches!(
        above.kind,
        javelin_classpath::DeclKind::Interface | javelin_classpath::DeclKind::AnnotationType
    ) {
        return true;
    }
    let package = |ty: &WorkspaceType| index.file_of(&ty.file).map(|file| file.package.clone());
    package(above).is_some_and(|package_above| Some(package_above) == package(below))
}

/// The one workspace type of this name; two is ambiguity.
fn one_type<'a>(index: &'a WorkspaceIndex, fqn: &str) -> Option<&'a WorkspaceType> {
    let mut declared = index.with_fqn(fqn);
    match (declared.next(), declared.next()) {
        (Some(one), None) => Some(one),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::testing::{Sources, indexed};
    use std::sync::Arc;

    fn externals(sources: &Sources) -> ClasspathExternals {
        ClasspathExternals::new(
            Arc::new(javelin_classpath::TypeWorld::empty()),
            Arc::clone(&sources.index),
        )
    }

    fn fqns(types: &[&WorkspaceType]) -> Vec<String> {
        let mut fqns: Vec<String> = types.iter().map(|ty| ty.fqn.clone()).collect();
        fqns.sort();
        fqns
    }

    fn shapes() -> Sources {
        indexed(
            "hierarchy-shapes",
            &[
                (
                    "p/Shape.java",
                    "package p;\npublic interface Shape { double area(); }\n",
                ),
                (
                    "p/Circle.java",
                    "package p;\npublic class Circle implements Shape { public double area() { return 0; } }\n",
                ),
                (
                    "p/Big.java",
                    "package p;\npublic class Big extends Circle {}\n",
                ),
                (
                    "q/Square.java",
                    "package q;\nimport p.Shape;\npublic class Square implements Shape { public double area() { return 1; } }\n",
                ),
                // Package `r` imports nothing: its `Shape` means nothing.
                (
                    "r/Fake.java",
                    "package r;\npublic class Fake implements Shape {}\n",
                ),
            ],
        )
    }

    #[test]
    fn a_subtype_is_one_whose_written_supertype_resolves_to_the_type() {
        let sources = shapes();
        let externals = externals(&sources);
        let index = &sources.index;
        assert_eq!(
            fqns(&direct_subtypes(&externals, index, "p.Shape")),
            vec!["p.Circle", "q.Square"]
        );
        assert_eq!(
            fqns(&all_subtypes(&externals, index, "p.Shape")),
            vec!["p.Big", "p.Circle", "q.Square"]
        );
        let big = index.with_fqn("p.Big").next().expect("Big");
        assert_eq!(
            fqns(&direct_supertypes(&externals, index, big)),
            vec!["p.Circle"]
        );
    }

    #[test]
    fn an_override_matches_by_name_arity_and_resolved_parameters() {
        let sources = shapes();
        let externals = externals(&sources);
        let index = &sources.index;
        let shape = index
            .members_of("p.Shape")
            .first()
            .expect("area")
            .name_start;
        let area = member(&externals, "p.Shape", shape).expect("Shape.area");
        let found: Vec<String> = overrides(&externals, index, "p.Shape", &area)
            .into_iter()
            .map(|(ty, _)| ty.fqn.clone())
            .collect();
        assert_eq!(
            found,
            vec!["p.Circle", "q.Square"],
            "Big declares no area of its own"
        );
    }

    #[test]
    fn a_type_variable_parameter_matches_and_two_candidates_match_neither() {
        let sources = indexed(
            "hierarchy-generic",
            &[
                (
                    "s/Store.java",
                    "package s;\npublic interface Store<T> { void put(T value); }\n",
                ),
                (
                    "s/Disk.java",
                    "package s;\npublic class Disk implements Store<String> { public void put(String v) {} }\n",
                ),
                (
                    "s/Mem.java",
                    "package s;\npublic class Mem implements Store<String> {\n    \
                     public void put(String v) {}\n    public void put(Integer v) {}\n}\n",
                ),
            ],
        );
        let externals = externals(&sources);
        let index = &sources.index;
        let start = index.members_of("s.Store").first().expect("put").name_start;
        let put = member(&externals, "s.Store", start).expect("Store.put");
        let found: Vec<String> = overrides(&externals, index, "s.Store", &put)
            .into_iter()
            .map(|(ty, _)| ty.fqn.clone())
            .collect();
        assert_eq!(
            found,
            vec!["s.Disk"],
            "Mem's two same-arity puts are ambiguity"
        );
    }

    fn overriders(sources: &Sources, owner: &str) -> Vec<String> {
        let externals = externals(sources);
        let index = &sources.index;
        let start = index
            .members_of(owner)
            .first()
            .expect("a method")
            .name_start;
        let method = member(&externals, owner, start).expect("the method");
        overrides(&externals, index, owner, &method)
            .into_iter()
            .map(|(ty, _)| ty.fqn.clone())
            .collect()
    }

    /// An array stays unresolved, so a rule that let any unresolved parameter
    /// match would list `write(String)` as overriding `write(byte[])`.
    #[test]
    fn an_array_parameter_matches_only_the_same_array() {
        let sources = indexed(
            "hierarchy-array",
            &[
                (
                    "a/Sink.java",
                    "package a;\npublic interface Sink { void write(byte[] d); }\n",
                ),
                (
                    "a/FileSink.java",
                    "package a;\npublic class FileSink implements Sink { public void write(byte[] d) {} }\n",
                ),
                (
                    "a/Special.java",
                    "package a;\npublic class Special extends FileSink { public void write(String s) {} }\n",
                ),
            ],
        );
        assert_eq!(overriders(&sources, "a.Sink"), vec!["a.FileSink"]);
    }

    #[test]
    fn a_variadic_method_is_overridden_only_by_a_variadic_one() {
        let sources = indexed(
            "hierarchy-varargs",
            &[
                (
                    "v/I.java",
                    "package v;\npublic interface I { void m(String... a); }\n",
                ),
                (
                    "v/C.java",
                    "package v;\npublic class C implements I { public void m(String... a) {} }\n",
                ),
                (
                    "v/D.java",
                    "package v;\npublic class D extends C { public void m(String a) {} }\n",
                ),
            ],
        );
        assert_eq!(overriders(&sources, "v.I"), vec!["v.C"]);
    }

    #[test]
    fn a_resolved_parameter_must_be_equal_not_merely_related() {
        let sources = indexed(
            "hierarchy-resolved",
            &[
                ("r/Shape.java", "package r;\npublic interface Shape {}\n"),
                (
                    "r/Circle.java",
                    "package r;\npublic class Circle implements Shape {}\n",
                ),
                (
                    "r/Visitor.java",
                    "package r;\npublic interface Visitor { void visit(Shape s); }\n",
                ),
                (
                    "r/V.java",
                    "package r;\npublic class V implements Visitor { public void visit(Shape s) {} }\n",
                ),
                (
                    "r/W.java",
                    "package r;\npublic class W extends V { public void visit(Circle c) {} }\n",
                ),
            ],
        );
        assert_eq!(overriders(&sources, "r.Visitor"), vec!["r.V"]);
    }

    #[test]
    fn a_type_that_extends_itself_is_neither_its_own_subtype_nor_supertype() {
        let sources = indexed(
            "hierarchy-self",
            &[("z/A.java", "package z;\npublic class A extends A {}\n")],
        );
        let externals = externals(&sources);
        let index = &sources.index;
        assert!(direct_subtypes(&externals, index, "z.A").is_empty());
        let a = index.with_fqn("z.A").next().expect("A");
        assert!(direct_supertypes(&externals, index, a).is_empty());
    }
}
