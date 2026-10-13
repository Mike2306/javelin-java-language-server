//! The in-memory type world: every JDK type fact, indexed for the questions
//! the request path asks — does a fully-qualified name exist, can one type be
//! a subtype of another, and what does a type declare, which the server's
//! member walk reads a fact at a time. `javelin-index` reads the fact store
//! from SQLite exactly once at warm start and builds one `TypeWorld` from it;
//! the server shares it as an `Arc` and answers every keystroke against that
//! shared, immutable copy. Nothing here mutates after `new`.

use std::collections::{HashMap, HashSet};

use crate::resolve::TypeTable;
use crate::sources::TypeFacts;

/// Every type fact this world knows, indexed for lookup by fully-qualified
/// name. Plain owned data: no interning, no cached answers, nothing behind a
/// lock. `facts` is the only owner of the fact strings; the index holds
/// positions into it.
#[derive(Debug)]
pub struct TypeWorld {
    facts: Vec<TypeFacts>,
    by_fqn: HashMap<String, usize>,
    /// How many of `facts` are the platform's — the JDK's, which is what
    /// [`TypeWorld::new`] indexes. The rest came from
    /// [`TypeWorld::extended`] and are never used to answer subtyping.
    platform: usize,
    /// The dependency types only a JAR the workspace's own build wrote holds
    /// — see [`TypeWorld::only_own_output`].
    own_output: HashSet<String>,
    /// The dependency names two JARs declare with different APIs — see
    /// [`TypeWorld::rivalled`].
    rivalled: HashSet<String>,
}

impl TypeWorld {
    /// How many nodes a closure walk over these facts visits before it gives
    /// up rather than walking an unbounded one. Same shape and the same number
    /// as `javelin_semantic::apply`'s file-local walk, but reached far more
    /// often — this one runs against the whole JDK, not one open buffer's
    /// supertypes.
    ///
    /// Public because [`TypeWorld::is_subtype`] is not the only walk over
    /// these facts any more: `javelin-server`'s member closure spends the same
    /// budget, and two numbers that had to agree would eventually not.
    pub const MAX_VISITS: usize = 64;

    /// The one fully-qualified name this file spells out — see
    /// [`TypeWorld::is_subtype`] for why it is not a `java.lang` table, and
    /// public for the same reason [`TypeWorld::MAX_VISITS`] is.
    pub const OBJECT: &'static str = "java.lang.Object";

    /// Indexes `facts`, which this world owns from here on.
    ///
    /// A repeated fqn keeps the *first* fact and drops the rest, because
    /// `javelin-index`'s writer does — two JDK modules declaring the same
    /// class is a real shape in some builds. Whichever rule this picked, the
    /// two have to pick the same one: a cold session builds this world from
    /// the archive and every warm session builds it from the store, and a
    /// disagreement would resolve the same call one way before the cache is
    /// written and the other way after, with nothing in the log to say why.
    pub fn new(facts: Vec<TypeFacts>) -> Self {
        let mut by_fqn = HashMap::with_capacity(facts.len());
        for (index, fact) in facts.iter().enumerate() {
            by_fqn.entry(fact.fqn.clone()).or_insert(index);
        }
        let platform = facts.len();
        Self {
            facts,
            by_fqn,
            platform,
            own_output: HashSet::new(),
            rivalled: HashSet::new(),
        }
    }

    /// This world's facts followed by `more`: the JDK's world extended with
    /// the dependencies'. A name `more` repeats keeps this world's fact, by
    /// [`TypeWorld::new`]'s first-one-wins rule.
    ///
    /// The added facts answer names and members and never subtyping: a
    /// dependency JAR can be a stale compiled copy of the workspace's own
    /// module, whose supertypes lead back into the open buffer — which
    /// overload elimination's JDK-only arms assume no external fact does.
    ///
    /// ponytail: every platform fact is cloned, so peak memory is twice the
    /// JDK's world plus the dependencies'. The upgrade is a layered world
    /// holding the platform behind an `Arc`.
    pub fn extended(&self, more: Vec<TypeFacts>) -> Self {
        let mut facts = Vec::with_capacity(self.facts.len() + more.len());
        facts.extend(self.facts.iter().cloned());
        facts.extend(more);
        let mut world = Self::new(facts);
        world.platform = self.platform;
        // Two JARs declaring one name with different APIs: the classpath is
        // the union of every module's, main's and test's, so two versions of
        // a library can both be on it, and which one a file compiles against
        // is not the first one's to say. The platform's fact stays whole.
        let mut doubted = Vec::new();
        for (index, fact) in world.facts.iter().enumerate().skip(world.platform) {
            let kept = world.by_fqn[&fact.fqn];
            if kept != index && kept >= world.platform && !same_api(&world.facts[kept], fact) {
                doubted.push(kept);
            }
        }
        for kept in doubted {
            world.facts[kept].supertypes_complete = false;
            world.facts[kept].members_complete = false;
            world.rivalled.insert(world.facts[kept].fqn.clone());
        }
        world
    }

    /// Whether two JARs declare `fqn` with different APIs, so the fact kept
    /// — its supertypes and where its source is — may be another version's
    /// than the one a file compiles against.
    pub fn rivalled(&self, fqn: &str) -> bool {
        self.rivalled.contains(fqn)
    }

    /// This world, knowing that `fqns` are held by no JAR but one the
    /// workspace's own build wrote.
    pub fn with_own_output(mut self, fqns: HashSet<String>) -> Self {
        self.own_output = fqns;
        self
    }

    /// Whether `fqn`'s fact came from the workspace's own build output and
    /// from nowhere else: a compiled copy of a source the workspace holds,
    /// which a multi-module build hands from one module to the next — so no
    /// rival to that source, as a vendored or stub copy would be.
    pub fn only_own_output(&self, fqn: &str) -> bool {
        self.own_output.contains(fqn) && !self.is_platform(fqn)
    }

    /// No JDK found, indexing not finished, or the classpath disabled —
    /// every question then answers "cannot say". Cheap enough to build that
    /// a request can be answered against it before the real world lands.
    pub fn empty() -> Self {
        Self {
            facts: Vec::new(),
            by_fqn: HashMap::new(),
            platform: 0,
            own_output: HashSet::new(),
            rivalled: HashSet::new(),
        }
    }

    /// Whether this world was built over a platform's facts — a JDK's — and
    /// not only over what [`TypeWorld::extended`] adds.
    pub fn has_platform(&self) -> bool {
        self.platform > 0
    }

    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }

    pub fn len(&self) -> usize {
        self.facts.len()
    }

    /// Whether some type this world holds is named `prefix` or lies under it
    /// — what an on-demand import needs to name for its package to exist.
    ///
    /// ponytail: a scan of every fact, for the diagnostics worker's few
    /// on-demand imports per file. The upgrade is a prefix set built once.
    pub fn holds_prefix(&self, prefix: &str) -> bool {
        self.facts.iter().any(|fact| {
            fact.fqn
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
        })
    }

    pub fn facts(&self, fqn: &str) -> Option<&TypeFacts> {
        self.by_fqn.get(fqn).map(|&index| &self.facts[index])
    }

    /// A depth-first walk of `from`'s direct supertypes outward, stack-based
    /// like `apply::may_be_subtype` — visit order does not affect the
    /// answer, since `Some(false)` requires the whole closure regardless of
    /// which order it was seen in.
    ///
    /// `Some(true)` when `to` is reached. `Some(false)` when the walk
    /// exhausts a *complete* closure — every node visited had
    /// `supertypes_complete == true` and a resolvable fqn for each of its
    /// supertypes — without reaching `to`. `None` otherwise: `from` or `to`
    /// unknown to the platform, some node the walk had to visit has an
    /// incomplete supertype list (including one naming a supertype this
    /// world has no fact for — the walk cannot continue past it, so it
    /// cannot have seen the whole closure either), or the visit budget ran
    /// out first.
    ///
    /// `is_subtype(x, x)` is `Some(true)` for a known `x` unconditionally: an
    /// incomplete or budget-exhausted closure above `x` says nothing about
    /// whether `x` is its own subtype. So is `is_subtype(x, Object)`, for a
    /// reason no closure can supply either.
    ///
    /// Only the platform's facts count — see [`TypeWorld::extended`]: a
    /// dependency's fact is as unknown here as a name the world never held.
    /// `to` need not be the platform's, but it must be a fact this world
    /// holds: a platform type's whole closure names only platform types, so a
    /// dependency's `to` is outside it — `String` is no `org.slf4j.Marker`. A
    /// name nothing holds answers `None`, because it may be a platform type a
    /// newer JDK than the one read declares, such as `SequencedCollection` to
    /// a JDK 17 world.
    pub fn is_subtype(&self, from: &str, to: &str) -> Option<bool> {
        self.is_subtype_beside(from, to, |_| false)
    }

    /// [`TypeWorld::is_subtype`], with `held` naming the types declared
    /// outside this world — the workspace's own — which a platform type's
    /// closure cannot reach either.
    pub fn is_subtype_beside(
        &self,
        from: &str,
        to: &str,
        held: impl Fn(&str) -> bool,
    ) -> Option<bool> {
        let from_index = self.platform_index(from)?;
        let to_index = self.platform_index(to);
        if to_index == Some(from_index) {
            return Some(true);
        }
        // JLS 4.10.2: every reference type is a subtype of `Object`, and this
        // world holds nothing but reference types — a primitive has no fact.
        // Nobody writes `extends Object`, so no closure here contains it for
        // the walk to reach: only a class gets it synthesised, which left an
        // interface, an enum, a record and an annotation type draining a
        // closure they believed complete and answering `Some(false)` to the
        // most common parameter type in Java.
        //
        // The name has the same standing as `javelin_semantic::apply`'s
        // `OBJECT`: a query key, never a name being interpreted. An fqn
        // cannot be shadowed, so this is not the `java.lang` table Phase 3a
        // rejected.
        if to == Self::OBJECT {
            return Some(true);
        }
        if to_index.is_none() && !self.by_fqn.contains_key(to) && !held(to) {
            return None;
        }

        let mut seen: Vec<usize> = Vec::new();
        let mut stack = vec![from_index];
        while let Some(current) = stack.pop() {
            // A cycle is invalid Java but ordinary input from a real archive
            // or a hand-built fact set; drop the repeat rather than truncate
            // the walk, since everything it could reach is already stacked.
            if seen.contains(&current) {
                continue;
            }
            if seen.len() >= Self::MAX_VISITS {
                return None;
            }
            seen.push(current);

            let fact = &self.facts[current];
            if !fact.supertypes_complete {
                return None;
            }
            for supertype in &fact.supertypes {
                let Some(index) = self.platform_index(supertype) else {
                    // Named by a resolved, "complete" supertype list, but not a
                    // fact this world holds — a dropped or unread entry.
                    // `simple_name`'s tier 1 (a single-type import) resolves
                    // without consulting `table.contains`, so a genuinely
                    // complete fact can still name an fqn this particular
                    // world never read (skipped for size, unparseable, or
                    // simply outside a partial index). Without it the closure
                    // above `from` cannot be shown whole, so this is exactly
                    // as unknowable as an incomplete node.
                    return None;
                };
                if Some(index) == to_index {
                    return Some(true);
                }
                stack.push(index);
            }
        }
        Some(false)
    }
}

impl TypeWorld {
    /// Whether `fqn`'s fact is the platform's rather than one
    /// [`TypeWorld::extended`] added.
    pub fn is_platform(&self, fqn: &str) -> bool {
        self.platform_index(fqn).is_some()
    }

    fn platform_index(&self, fqn: &str) -> Option<usize> {
        self.by_fqn
            .get(fqn)
            .copied()
            .filter(|&index| index < self.platform)
    }
}

impl TypeTable for TypeWorld {
    fn contains(&self, fqn: &str) -> bool {
        self.by_fqn.contains_key(fqn)
    }
}

/// Whether two facts for one name declare the same API — where it came from
/// aside.
fn same_api(one: &TypeFacts, other: &TypeFacts) -> bool {
    one.kind == other.kind
        && one.type_parameters == other.type_parameters
        && one.supertypes == other.supertypes
        && one.supertype_arguments == other.supertype_arguments
        && one.supertypes_complete == other.supertypes_complete
        && one.members_complete == other.members_complete
        && one.members.len() == other.members.len()
        // Where a member is written is not its API: the same class from a JAR
        // with its sources beside it and one without is one API.
        && one.members.iter().zip(&other.members).all(|(a, b)| {
            a.name == b.name
                && a.kind == b.kind
                && a.is_static == b.is_static
                && a.is_abstract == b.is_abstract
                && a.is_public == b.is_public
                && a.declared == b.declared
                && a.parameters == b.parameters
                && a.variadic == b.variadic
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use javelin_parser::DeclKind;

    fn fact(fqn: &str, supertypes: &[&str], supertypes_complete: bool) -> TypeFacts {
        let owned = supertypes.iter().map(|s| (*s).to_owned()).collect();
        fact_owned(fqn, owned, supertypes_complete)
    }

    fn interface(fqn: &str, supertypes: &[&str]) -> TypeFacts {
        TypeFacts {
            name_start: 0,
            name_end: 0,
            kind: DeclKind::Interface,
            ..fact(fqn, supertypes, true)
        }
    }

    fn fact_owned(fqn: &str, supertypes: Vec<String>, supertypes_complete: bool) -> TypeFacts {
        let simple = fqn.rsplit('.').next().unwrap_or(fqn).to_owned();
        TypeFacts {
            constructors: Vec::new(),
            doc_span: None,
            name_start: 0,
            name_end: 0,
            fqn: fqn.to_owned(),
            simple,
            kind: DeclKind::Class,
            type_parameters: Vec::new(),
            supertype_arguments: Vec::new(),
            supertypes,
            supertypes_complete,
            members_complete: true,
            members: Vec::new(),
            // Hand-built, so there is no zip entry to name.
            source: String::new(),
        }
    }

    /// A JAR can hold a stale compiled copy of the workspace's own module —
    /// `mvn install` puts one in `~/.m2` and the classpath names it — so a
    /// dependency fact can lead back into the open buffer, which is what
    /// `apply.rs`'s two JDK-only arms assume cannot happen. Subtyping is
    /// answered from the platform the world was built over and never from
    /// what `extended` added.
    #[test]
    fn subtyping_is_answered_from_the_platform_facts_alone() {
        let jdk = TypeWorld::new(vec![
            fact("java.lang.Object", &[], true),
            fact("java.lang.Number", &["java.lang.Object"], true),
        ]);
        let world = jdk.extended(vec![
            fact("com.acme.Handler", &["java.lang.Object"], true),
            fact("com.acme.Default", &["java.lang.Number"], true),
        ]);
        assert_eq!(
            world.is_subtype("java.lang.Number", "java.lang.Object"),
            Some(true)
        );
        assert_eq!(
            world.is_subtype("com.acme.Default", "java.lang.Number"),
            None
        );
        assert_eq!(
            world.is_subtype("com.acme.Handler", "com.acme.Handler"),
            None
        );
        // A platform type's whole closure is the platform's, so no JAR type
        // is in it.
        assert_eq!(
            world.is_subtype("java.lang.Number", "com.acme.Handler"),
            Some(false)
        );
        assert!(
            world.facts("com.acme.Default").is_some(),
            "still held for members"
        );
        let dependencies_only =
            TypeWorld::empty().extended(vec![fact("com.acme.Handler", &[], true)]);
        assert_eq!(
            dependencies_only.is_subtype("com.acme.Handler", "com.acme.Handler"),
            None
        );
    }

    /// The JDK's facts come first, so a JAR that vendors a JDK class does not
    /// replace it, and the first of two JARs declaring a name keeps it — the
    /// compiler's rule for `java.*` and the class loader's for the rest.
    #[test]
    fn an_extended_world_keeps_the_first_fact_for_a_repeated_name() {
        let jdk = TypeWorld::new(vec![fact("java.lang.String", &["java.lang.Object"], true)]);
        let world = jdk.extended(vec![
            fact("java.lang.String", &[], false),
            fact("com.acme.Box", &["java.lang.Object"], true),
            fact("com.acme.Box", &["java.lang.Object"], true),
        ]);
        assert_eq!(world.len(), 4);
        let string = world.facts("java.lang.String").expect("held");
        assert!(string.supertypes_complete && string.members_complete);
        let boxed = world.facts("com.acme.Box").expect("held");
        assert!(
            boxed.supertypes_complete && boxed.members_complete,
            "two copies of one API are one API"
        );
        assert!(!world.rivalled("com.acme.Box") && !world.rivalled("java.lang.String"));
        assert_eq!(jdk.len(), 1, "the JDK's own world is unchanged");
    }

    /// The classpath is the union of every module's, main's and test's, so
    /// two versions of one library can both be on it, and which one a file
    /// compiles against is not the first one's to say.
    /// The same class from a JAR with its sources beside it and from one
    /// without: where its members are written is not its API.
    #[test]
    fn one_class_sited_and_unsited_is_one_api() {
        let method = crate::MemberFacts {
            type_parameters: Vec::new(),
            doc_span: None,
            parameter_names: Vec::new(),
            name: "run".to_owned(),
            kind: DeclKind::Method,
            is_static: false,
            is_abstract: false,
            is_public: true,
            declared: crate::MemberType::Void,
            parameters: Vec::new(),
            variadic: false,
            name_start: 0,
            name_end: 0,
        };
        let bare = TypeFacts {
            members: vec![method.clone()],
            ..fact("com.acme.Task", &["java.lang.Object"], true)
        };
        let sited = TypeFacts {
            members: vec![crate::MemberFacts {
                name_start: 10,
                name_end: 13,
                ..method
            }],
            source: "/m2/acme-sources.jar!/com/acme/Task.java".to_owned(),
            name_start: 4,
            name_end: 8,
            ..bare.clone()
        };
        let world =
            TypeWorld::new(vec![fact("java.lang.Object", &[], true)]).extended(vec![sited, bare]);
        let task = world.facts("com.acme.Task").expect("held");
        assert!(task.members_complete && task.supertypes_complete);
    }

    #[test]
    fn two_jars_declaring_one_name_differently_leave_it_incomplete() {
        let world = TypeWorld::new(vec![fact("java.lang.Object", &[], true)]).extended(vec![
            fact("org.slf4j.Logger", &[], true),
            fact("org.slf4j.Logger", &["java.lang.Object"], true),
        ]);
        let logger = world.facts("org.slf4j.Logger").expect("held");
        assert!(!logger.supertypes_complete);
        assert!(!logger.members_complete);
        assert!(world.rivalled("org.slf4j.Logger"));
    }

    #[test]
    fn a_type_is_a_subtype_of_itself_and_of_its_declared_supertypes() {
        let world = TypeWorld::new(vec![
            fact("com.example.Base", &[], true),
            fact("com.example.Sub", &["com.example.Base"], true),
        ]);

        assert_eq!(
            world.is_subtype("com.example.Sub", "com.example.Sub"),
            Some(true)
        );
        assert_eq!(
            world.is_subtype("com.example.Sub", "com.example.Base"),
            Some(true)
        );
    }

    #[test]
    fn an_unrelated_type_is_demonstrably_not_a_subtype() {
        let world = TypeWorld::new(vec![
            fact("com.example.A", &[], true),
            fact("com.example.B", &[], true),
        ]);

        assert_eq!(
            world.is_subtype("com.example.A", "com.example.B"),
            Some(false)
        );
    }

    #[test]
    fn a_type_whose_closure_is_incomplete_cannot_say() {
        let world = TypeWorld::new(vec![
            fact("com.example.Foo", &[], false),
            fact("com.example.Bar", &[], true),
        ]);

        assert_eq!(world.is_subtype("com.example.Foo", "com.example.Bar"), None);
    }

    #[test]
    fn a_type_the_world_does_not_know_cannot_say() {
        let world = TypeWorld::new(vec![fact("java.util.List", &[], true)]);

        assert_eq!(
            world.is_subtype("com.example.Unknown", "java.util.List"),
            None
        );
        // `List`'s whole closure is known and does not name it, but a JDK
        // newer than the one read may: `SequencedCollection` is one to JDK 17.
        assert_eq!(
            world.is_subtype("java.util.List", "java.util.SequencedCollection"),
            None
        );
    }

    #[test]
    fn a_supertype_naming_an_unread_type_cannot_say() {
        // `supertypes_complete` is true — every written supertype resolved to
        // exactly one name — but that name is not itself a fact this world
        // holds. `simple_name`'s tier 1 resolves a single-type import without
        // consulting `table.contains`, so this is a real shape, not a
        // defensive one: the walk cannot show the closure above `Foo` whole
        // without a fact for `com.example.Unread`, so it must answer `None`
        // rather than continue past the gap to a wrong `Some(false)`.
        let world = TypeWorld::new(vec![
            fact("com.example.Foo", &["com.example.Unread"], true),
            fact("com.example.Bar", &[], true),
        ]);

        assert_eq!(world.is_subtype("com.example.Foo", "com.example.Bar"), None);
    }

    #[test]
    fn a_supertype_cycle_terminates_instead_of_hanging() {
        let world = TypeWorld::new(vec![
            fact("com.example.A", &["com.example.B"], true),
            fact("com.example.B", &["com.example.A"], true),
            fact("com.example.C", &[], true),
        ]);

        assert_eq!(
            world.is_subtype("com.example.A", "com.example.C"),
            Some(false)
        );
    }

    #[test]
    fn a_closure_deeper_than_the_budget_cannot_say() {
        // MAX_VISITS is 64; a chain of 100 nodes forces the walk to give up
        // before it can see the whole thing, whether or not the target is
        // actually in the chain.
        let depth = 100;
        let facts: Vec<TypeFacts> = (0..depth)
            .map(|i| {
                let supertypes = if i + 1 < depth {
                    vec![format!("com.example.Link{}", i + 1)]
                } else {
                    Vec::new()
                };
                fact_owned(&format!("com.example.Link{i}"), supertypes, true)
            })
            .collect();
        let world = TypeWorld::new(facts);

        assert_eq!(
            world.is_subtype("com.example.Link0", "com.example.NotThere"),
            None
        );
    }

    #[test]
    fn the_empty_world_cannot_say_anything() {
        let world = TypeWorld::empty();

        assert!(world.is_empty());
        assert_eq!(world.len(), 0);
        assert_eq!(world.facts("com.example.Foo"), None);
        assert!(!world.contains("com.example.Foo"));
        assert_eq!(world.is_subtype("com.example.Foo", "com.example.Foo"), None);
    }

    #[test]
    fn every_reference_type_is_a_subtype_of_object() {
        // An interface's source is `public interface Serializable {}`: no
        // `extends`, so `resolve_facts` synthesises nothing and the fact is
        // `supertypes: [], supertypes_complete: true`. Walking that closure
        // finds no `Object` and would drain to `Some(false)`, which excludes
        // a `Serializable` argument from an `Object` parameter and picks the
        // wrong overload.
        let world = TypeWorld::new(vec![
            fact("java.lang.Object", &[], true),
            interface("java.io.Serializable", &[]),
            interface("java.lang.Comparable", &[]),
        ]);

        assert_eq!(
            world.is_subtype("java.io.Serializable", "java.lang.Object"),
            Some(true)
        );
        // And it is the target that gets the exemption, not the source: an
        // interface is still demonstrably not another unrelated interface.
        assert_eq!(
            world.is_subtype("java.io.Serializable", "java.lang.Comparable"),
            Some(false)
        );
    }

    #[test]
    fn a_duplicate_fully_qualified_name_keeps_the_first_fact() {
        // `javelin-index`'s writer drops every repeat, so the first row for an
        // fqn is what a warm session reads back. A cold session builds this
        // world straight from the archive, and the two must agree or the same
        // call resolves one way before the cache is written and another after.
        let world = TypeWorld::new(vec![
            fact("com.example.Dup", &["com.example.First"], true),
            fact("com.example.First", &[], true),
            fact("com.example.Dup", &["com.example.Second"], true),
            fact("com.example.Second", &[], true),
        ]);

        assert_eq!(
            world
                .facts("com.example.Dup")
                .map(|f| f.supertypes.as_slice()),
            Some(["com.example.First".to_owned()].as_slice())
        );
        assert_eq!(
            world.is_subtype("com.example.Dup", "com.example.Second"),
            Some(false)
        );
    }

    #[test]
    fn an_interface_reached_through_a_superclass_is_a_supertype() {
        let world = TypeWorld::new(vec![
            fact("com.example.Iface", &[], true),
            fact("com.example.Base", &["com.example.Iface"], true),
            fact("com.example.Sub", &["com.example.Base"], true),
        ]);

        assert_eq!(
            world.is_subtype("com.example.Sub", "com.example.Iface"),
            Some(true)
        );
    }
}
