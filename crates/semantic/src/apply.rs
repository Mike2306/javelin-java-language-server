//! Whether an argument demonstrably cannot be passed to a parameter.
//!
//! The subset of JLS 15.12 implemented here is applicability by subtyping
//! (15.12.2.2), by widening primitive conversion (5.1.2) and, wherever a type
//! fact settles it, by boxing and unboxing (5.1.7, 5.1.8). It runs as
//! *elimination*: a candidate no argument rules out survives. Two survivors
//! are an ambiguity unless [`most_specific`] can *prove* one of them is
//! JLS 15.12.2.5's answer — by strict invocation alone, over a list the walk
//! shows whole — and otherwise still answer nothing.
//!
//! Only exclusion is claimed, never applicability. `false` is the answer
//! whenever the question cannot be settled, and an unsound `true` picks the
//! wrong overload, which is worse than picking none. What *can* be settled is
//! now a property of the world the server hands down rather than of the
//! buffer alone. With a JDK indexed, `f(String)` at `f(1)` is excluded: `int`
//! boxes to `java.lang.Integer` and no fact makes an `Integer` a `String`. With
//! [`Externals`] answering "cannot say" — no JDK found, indexing unfinished,
//! the classpath switched off — every answer here is exactly Phase 2's, which
//! had to keep that candidate because an unresolvable `String` could have been
//! `Object`, `Number` or `Comparable`, every one of which takes an `int`.
//!
//! Refused either way, and none of it approximated: most-specific-method
//! selection past strict invocation, so `f(Integer)` against `f(Object)` at
//! `f(1)` — boxing, the second phase — leaves two *applicable* candidates and
//! answers nothing, where `f(int)` against `f(long)` picks `f(int)`; type
//! arguments, so `Box<Foo>` and `Box<Bar>`
//! are one declaration; a variadic candidate's tail; a poly expression, whose
//! type comes from the very choice being made; an enum, a record or an
//! annotation type the classpath holds, as the *argument*, because the
//! supertype JLS 8.9, 8.10 and 9.6 give those three is a *named* type nobody
//! writes down, so the closure the world would have to drain is missing part
//! of itself — a reachable file's one is separable wherever the world holds
//! that supertype, since the walk asks for it by name. As a *parameter* type
//! an external one of the three is still separable, because the closure
//! walked belongs to the argument: `f(DayOfWeek)` at `f(s)` for a `String` `s` is excluded. An
//! interface is not in the group at all, in either position, because its own
//! unwritten supertype is [`OBJECT`] alone. And a generic override the subtype
//! pinned down, which is `Member::parameter_types`' ceiling rather than this
//! reasoning's.

use javelin_parser::DeclKind;

use crate::externals::Externals;
use crate::files::{DeclSite, Files};
use crate::members::{Member, is_type_kind};
use crate::resolve::resolve_written;
use crate::types::{Primitive, Type};

/// How many declarations the subtype walk may visit before it gives up and
/// answers "could be". Counted across every file the walk reaches, not per
/// file: a wide set spends the budget exactly as a tall hierarchy does.
const MAX_SUPERTYPES: usize = 64;

/// The fully-qualified name this crate leans on most.
///
/// Not the `java.lang` table Phase 3a rejected: that every class and interface
/// is a subtype of `java.lang.Object` is JLS 4.10.2, true without reading a
/// classpath, and unwritten in the source — so the supertype walk cannot see
/// it and would exclude every file-local argument from `f(Object)`, which is
/// the wrongest available pick. The name is still *asked* of the world rather
/// than trusted: a world that does not hold it says "cannot say", and that
/// excludes nothing.
const OBJECT: &str = "java.lang.Object";

/// What an array type is a subtype of besides another array, JLS 4.10.3.
const ARRAY_SUPERTYPES: [&str; 3] = [OBJECT, "java.lang.Cloneable", "java.io.Serializable"];

/// JLS 15.12.2.5's most specific method, where it can be *proven*: the one
/// candidate every argument reaches by strict invocation (15.12.2.2 —
/// identity, primitive widening, reference subtyping; no boxing, no
/// varargs) whose every parameter type is a subtype of the corresponding one
/// of each other survivor.
///
/// Sound against survivors that are not really applicable, which elimination
/// cannot rule out: the winner is applicable in Java's first phase, so that
/// phase's set holds it, every other member of the set is a survivor, and the
/// winner is strictly more specific than each. Only exact, *proven*
/// relations count — a type argument is compared only by equality, a
/// parameter type with arguments is reached only by an identical one, and
/// anything unknown or variable proves nothing — so `f(int)` against
/// `f(long)` at `f(1)` picks `f(int)`, `f(String)` against `f(Object)` at
/// `f(null)` picks `f(String)`, and `f(Integer)` against `f(int)` at `f(1)`
/// still picks nothing here (boxing is the second phase).
///
/// Refused outright: a variadic survivor (the third phase has rules of its
/// own) and a survivor whose parameter types do not all resolve; a generic
/// method's own variables prove nothing, so it never wins. The caller also
/// owes the list being whole, since a candidate the walk never reached may be
/// more specific.
///
/// ponytail: only Java's first phase is ranked, so a pick boxing or varargs
/// decide — `f(Integer)` against `f(Object)` at `f(1)` — answers nothing. The
/// upgrade is running the same proof per phase, which needs applicability
/// proven by boxing as well as refuted by it.
pub(crate) fn most_specific<'m, 'a>(
    matches: &'m [Member<'a>],
    arguments: &[Type],
    files: &Files<'_>,
    externals: &dyn Externals,
) -> Option<&'m Member<'a>> {
    let mut parameters = Vec::with_capacity(matches.len());
    for member in matches {
        if member.is_variadic() || member.parameter_count() != arguments.len() {
            return None;
        }
        parameters.push(member.parameter_types(files, externals)?);
    }
    let applicable = |own: &[Type]| {
        own.iter()
            .zip(arguments)
            .all(|(to, from)| proven_subtype(from, to, files, externals))
    };
    // Two survivors with identical parameters are each more specific than
    // the other, so neither is the one winner below.
    let more_specific = |own: &[Type], other: &[Type]| {
        own.iter()
            .zip(other)
            .all(|(from, to)| proven_subtype(from, to, files, externals))
    };
    let mut winners = (0..matches.len()).filter(|&at| {
        applicable(&parameters[at])
            && (0..matches.len())
                .all(|other| other == at || more_specific(&parameters[at], &parameters[other]))
    });
    match (winners.next(), winners.next()) {
        // Java's candidates are only those the caller can see (JLS
        // 15.12.2.1), so a winner it cannot see is not Java's answer — a
        // package-private `append(AbstractStringBuilder)` beats every public
        // `append` on paper. The others need not be visible: a survivor Java
        // never considers only makes the proof harder.
        (Some(at), None) if matches[at].visible_from(files) => Some(&matches[at]),
        _ => None,
    }
}

/// Whether `from` is demonstrably a subtype of `to` (JLS 4.10), or `null`
/// passable to it — the positive counterpart of [`cannot_pass`], and as
/// narrow as it must be to be a proof.
fn proven_subtype(from: &Type, to: &Type, files: &Files<'_>, externals: &dyn Externals) -> bool {
    let reference = |ty: &Type| {
        matches!(
            ty,
            Type::External { .. } | Type::Declared { .. } | Type::Array(_)
        )
    };
    if concrete(from) && from == to {
        return true;
    }
    match (from, to) {
        (Type::Primitive(from), Type::Primitive(to)) => widens(*from, *to),
        (Type::Null, to) => reference(to),
        // A parameterized target is reached only by an identical type, which
        // the equality above answered: type arguments are not compared.
        (_, Type::External { arguments, .. } | Type::Declared { arguments, .. })
            if !arguments.is_empty() =>
        {
            false
        }
        // JLS 4.10.2 and 4.10.3: every reference type reaches `Object`.
        (from, Type::External { fqn, .. }) if fqn == OBJECT && reference(from) => {
            known(OBJECT, externals)
        }
        (Type::Array(_), Type::External { fqn, .. }) => {
            ARRAY_SUPERTYPES.contains(&fqn.as_str()) && known(fqn, externals)
        }
        (Type::Array(from), Type::Array(to)) => {
            reference(from) && reference(to) && proven_subtype(from, to, files, externals)
        }
        // Against a raw or non-generic target the base types are the whole
        // question: every parameterization reaches the raw supertypes.
        (Type::External { fqn: from, .. }, Type::External { fqn: to, .. }) => {
            externals.is_subtype(from, to) == Some(true)
        }
        (Type::Declared { decl, .. }, Type::Declared { decl: to, .. }) => {
            proven_by_walk(decl.clone(), Target::Local(to), files, externals)
        }
        (Type::Declared { decl, .. }, Type::External { fqn, .. }) => {
            proven_by_walk(decl.clone(), Target::External(fqn), files, externals)
        }
        _ => false,
    }
}

/// A type with nothing unknown or variable anywhere in it, so equality with
/// another is identity.
pub(crate) fn concrete(ty: &Type) -> bool {
    match ty {
        Type::Primitive(_) => true,
        Type::Array(element) => concrete(element),
        Type::External { arguments, .. } | Type::Declared { arguments, .. } => {
            arguments.iter().all(concrete)
        }
        _ => false,
    }
}

/// Whether `from`'s written supertypes, each resolved where it was written,
/// reach `to` — a proof, so an unresolved or unreadable hop proves nothing
/// rather than everything, which is [`may_be_subtype`]'s opposite.
fn proven_by_walk(
    from: DeclSite,
    to: Target<'_>,
    files: &Files<'_>,
    externals: &dyn Externals,
) -> bool {
    let mut seen: Vec<DeclSite> = Vec::new();
    let mut queue = vec![from];
    while let Some(current) = queue.pop() {
        if matches!(to, Target::Local(to) if *to == current) {
            return true;
        }
        if seen.contains(&current) || seen.len() >= MAX_SUPERTYPES {
            continue;
        }
        let Some(view) = files.viewing(&current.file) else {
            continue;
        };
        let Some(decl) = view.current_syntax().declaration_at(current.span) else {
            continue;
        };
        seen.push(current);
        for written in decl.extends.iter().chain(decl.implements.iter()) {
            match resolve_written(written, &view, externals, decl.name_span.start_byte) {
                Type::Declared { decl, .. } => queue.push(decl),
                Type::External { fqn, .. } => {
                    if let Target::External(to) = to
                        && externals.is_subtype(&fqn, to) == Some(true)
                    {
                        return true;
                    }
                }
                _ => {}
            }
        }
    }
    false
}

/// Whether any argument demonstrably cannot be passed to the parameter it
/// lands on, which rules this candidate out.
///
/// Argument `i` maps to parameter `i`. A trailing variadic parameter is not
/// checked at all: the candidate is judged on its fixed prefix only.
///
/// ponytail: the variadic tail goes unchecked, so `f(int, Foo...)` and
/// `f(int, Bar...)` are not separated by their tails. Checking one needs the
/// element type, and `Decl::declared_type` for a `...` parameter holds the
/// element as written with no `Type::Array` around it — the upgrade is to
/// unwrap that deliberately rather than to have it both ways by accident.
pub(crate) fn excluded_by_arguments(
    member: &Member<'_>,
    arguments: &[Type],
    files: &Files<'_>,
    externals: &dyn Externals,
) -> bool {
    let declared = member.parameter_count();
    // `saturating_sub`: for an external member the two questions read two
    // independent fields of a `pub` struct — a `variadic` flag and a
    // `parameters` vector — so nothing in the type stops a variadic member
    // arriving with no parameters, and a request must not panic on one.
    // `ExternalMember::signature` saturates on the same shape.
    let fixed = if member.is_variadic() {
        declared.saturating_sub(1)
    } else {
        declared
    };
    // `unwrap_or_default`: a candidate whose own file is unreachable has no
    // parameter type to compare an argument against, and no comparison excludes
    // nothing — the same answer an empty `zip` gives.
    member
        .parameter_types(files, externals)
        .unwrap_or_default()
        .iter()
        .take(fixed)
        // `zip` rather than an index: a buffer mid-keystroke has fewer
        // arguments than parameters often enough that the arity filter's floor
        // lets one through, and a missing argument excludes nothing.
        .zip(arguments)
        .enumerate()
        .any(|(index, (to, argument))| {
            cannot_pass(argument, to, files, externals)
                && !(matches!(to, Type::External { fqn, .. } if !known(fqn, externals))
                    && member.parameter_may_be_shadowed(index, files, externals))
        })
}

/// Whether `own` *hides* every other surviving candidate rather than competing
/// with it.
///
/// The own-shadows-inherited rule narrows by where a member was declared, and
/// that is only a demonstration when the own member overrides the inherited
/// one — which needs the parameter types to match, not merely the arity to fit.
/// `class Sub extends Base` declaring `f(Bar)` against `Base`'s `f(Foo)` is an
/// overload set, not an override, so an argument that separates neither must
/// answer nothing rather than `Sub`'s.
///
/// Equality is the whole test, and nothing weaker: two `Type::Unknown` carrying
/// the same written text in one compilation unit resolve the same way, so they
/// are equal and that is a demonstration.
///
/// A candidate on either side whose own file the set cannot reach hides
/// nothing and is hidden by nothing: `Member::parameter_types` answers `None`
/// there, and `None` is not equal to a list, so the only thing an unreachable
/// declaration can do here is stop a pick — never make one.
///
/// A type and a variable never hide each other (JLS 6.5.2 keeps them in two
/// namespaces), and both have no parameters to compare, so kind is asked first:
/// `Sub`'s nested `class g` beside `Base`'s field `g` is two answers, one per
/// position, and a value access cannot say which position it is in.
pub(crate) fn hides_the_rest(
    own: &Member<'_>,
    matches: &[Member<'_>],
    files: &Files<'_>,
    externals: &dyn Externals,
) -> bool {
    let Some(expected) = own.parameter_types(files, externals) else {
        return false;
    };
    let is_type = is_type_kind(own.kind());
    matches.iter().all(|member| {
        member.same_declaration(own)
            || (is_type_kind(member.kind()) == is_type
                && member.parameter_types(files, externals).as_ref() == Some(&expected))
    })
}

/// Whether an argument of type `from` demonstrably cannot be passed to a
/// parameter of type `to`.
///
/// `true` only for the certainties enumerated in the arms below. Everything
/// else — an unresolvable type on either side, a type variable, an array, a
/// poly expression's unknown — answers `false`, leaving the candidate standing.
fn cannot_pass(from: &Type, to: &Type, files: &Files<'_>, externals: &dyn Externals) -> bool {
    match (from, to) {
        // A `void` call in an argument position does not compile at all. It is
        // still ordinary input — the buffer is invalid Java most of the time it
        // is read — and excluding it is the answer rather than an assumption.
        (Type::Void, _) => true,
        // JLS 5.1.2: identical, or a widening primitive conversion. Modelling
        // widening is not politeness — without it `f(long)` at `f(1)` would be
        // excluded, which is an unsound `true` and picks the wrong overload.
        (Type::Primitive(from), Type::Primitive(to)) => !widens(*from, *to),
        // Boxing an `int` yields `java.lang.Integer`, whose supertypes are all
        // the JDK's, and a type any reachable file declares is not among them —
        // not even one such file names `Integer`, which is a different type from
        // `java.lang.Integer` and does not unbox. The reverse holds by the same
        // argument: a file-local class is not a box type.
        (Type::Primitive(_), Type::Declared { .. })
        | (Type::Declared { .. }, Type::Primitive(_)) => true,
        // `null` is passable to every reference type, including every
        // unresolvable one, and to no primitive.
        (Type::Null, Type::Primitive(_)) => true,
        (Type::Declared { decl: from, .. }, Type::Declared { decl: to, .. }) => {
            !may_be_subtype(from.clone(), Target::Local(to), files, externals)
        }
        // JLS 5.1.7: an `int` boxes to `java.lang.Integer` and to nothing else.
        // There is no widen-then-box conversion — `int` does not reach `Long` —
        // so the box type alone is the whole question. `boxed` answers `None`
        // for a world that does not hold the box type, which asks nothing.
        (Type::Primitive(from), Type::External { fqn: to, .. }) => externals
            .boxed(*from)
            .is_some_and(|boxed| externals.is_subtype(&boxed, to) == Some(false)),
        // JLS 5.1.8 and then 5.1.2: unboxing *may* be followed by a widening
        // primitive conversion, so an `Integer` reaches `long` but not `short`.
        // Excluding unless that chain is demonstrated is the one arm that
        // claims something from a negative answer, so it is gated on the world
        // holding `from` at all: a name it has never heard of unboxes to `None`
        // for want of a fact, not for want of a conversion.
        (Type::External { fqn: from, .. }, Type::Primitive(to)) => {
            known(from, externals)
                && !externals
                    .unboxed(from)
                    .is_some_and(|unboxed| widens(unboxed, *to))
        }
        // Wholly the world's answer, and `Some(false)` is the only one that
        // excludes. An external enum, record or annotation type as `from`
        // answers `None`, where the walk below asks the world on a local one's
        // behalf: the closure to be drained is `from`'s, and it is missing the
        // supertype the language gives it and nobody writes down. As `to` it
        // is as separable as anything else, because then the closure walked
        // belongs to the argument. An external interface is separable in
        // either position — [`OBJECT`] is the only thing missing from its
        // closure, and the world answers that without a fact.
        (Type::External { fqn: from, .. }, Type::External { fqn: to, .. }) => {
            externals.is_subtype(from, to) == Some(false)
        }
        // A type the JDK declares cannot extend a type declared in the buffer
        // the user is typing, so the world merely having heard of `from` is the
        // whole demonstration — no walk, and no need to know anything about the
        // target beyond where it was declared.
        //
        // ponytail: that holds only because `known` reads
        // [`Externals::is_subtype`], and the one implementation answers that
        // out of the JDK's facts alone. A `Type::External` can now name a
        // *sibling source file* — the server resolves a name against the
        // workspace index and the JDK through one table — and `class Other
        // extends Bar` in the file next door is exactly the shape this arm
        // would exclude wrongly. It does not, because a workspace fqn is not
        // `known`: `is_subtype` has never heard of it, so this answers
        // `false`, which excludes nothing. The refusing direction, bought by
        // keeping one half of the index behind the door — see
        // `ClasspathExternals`, which says so and says what it costs.
        //
        // What is still owed is the *upgrade*, and it is half landed: a fact
        // carries where it came from, the `src.zip` entry on
        // `javelin_classpath::TypeFacts`, but `Externals` answers about types
        // rather than about where they were read, so this arm cannot tell a
        // JDK fact from any other and has to be given only JDK ones. It
        // assumes, too, that the buffer is not itself one of the indexed
        // sources: with `java/util/AbstractList.java` open, a
        // `java.util.ArrayList` argument is excluded from a parameter the file
        // declares, which Java accepts.
        (Type::External { fqn: from, .. }, Type::Declared { .. }) => known(from, externals),
        (Type::Declared { decl: from, .. }, Type::External { fqn: to, .. }) => {
            !may_be_subtype(from.clone(), Target::External(to), files, externals)
        }
        // JLS 4.10.3: an array's supertypes are `Object`, `Cloneable` and
        // `Serializable`, and nothing but an array or `null` is an array — so
        // a class, an interface or a primitive never reaches one, and an
        // array reaches no primitive, no file-local type and no other
        // classpath type. Certain whatever the element type is.
        (Type::External { .. } | Type::Declared { .. } | Type::Primitive(_), Type::Array(_))
        | (Type::Array(_), Type::Primitive(_) | Type::Declared { .. }) => true,
        (Type::Array(_), Type::External { fqn, .. }) => !ARRAY_SUPERTYPES.contains(&fqn.as_str()),
        // ponytail: `Foo[]` against `Bar[]` is not separated. Array covariance
        // plus an element type that may be unresolvable makes the certainty
        // cost more to establish than the separation is worth. The upgrade is to recurse through
        // `Type::Array` once the element side is worth trusting.
        //
        // `null` against an external type falls here and excludes nothing, as
        // it does for every other reference type. So does a type variable on
        // either side, and an unresolvable name on either side — the answer
        // every pair had before there was a classpath to ask.
        _ => false,
    }
}

/// What a subtype walk is looking for.
///
/// Two targets and not one [`Type`], because the two are answered differently
/// at every external supertype the walk meets: a local target cannot be
/// reached *through* one, and an external target can only be reached through
/// one.
#[derive(Debug, Clone, Copy)]
enum Target<'a> {
    /// A declaration in one of the files the query can reach, by its site.
    Local(&'a DeclSite),
    /// A type only the classpath holds, by fully-qualified name.
    External(&'a str),
}

/// Whether `from`'s declaration could be a subtype of `to`.
///
/// Conservative in one direction only: `true` when `to` is reachable, and also
/// `true` whenever the walk cannot see far enough to say — a supertype nothing
/// resolves, an anonymous class, a budget exhausted, or a `None` from
/// [`Externals::is_subtype`], which means *cannot say* and never *no*. That
/// matters more than it looks.
/// `class Mid extends Other` where `Other` is another file's may still be a
/// subtype of an interface *this* file declares, because that other file can
/// implement it; excluding `Mid` there would be an unsound `true`.
///
/// What the classpath changed is the abort condition, and it is *unresolved*
/// rather than *external*: a supertype the classpath resolves is one the walk
/// can hand off to the world instead of giving up at. That is what closes the
/// largest refusal Phase 2 made — `class Foo implements Serializable` is now
/// separable from a file-local `Bar`, where before `Serializable` resolved to
/// nothing and every walk through it answered "could be".
///
/// ponytail: a second supertype walk beside the one inside `members_of`, with
/// its own cycle guard and budget. `members_of`'s carries substitutions and
/// yields members, so it is not reusable as it stands; the upgrade is to
/// extract the walk itself, or to intern types so a reachability question is a
/// pointer comparison.
///
/// Type arguments are not compared: `Box<Foo>` and `Box<Bar>` are the same
/// declaration and neither excludes the other. Comparing them needs
/// `members_of`'s substitution machinery and separates nothing a real file
/// needs.
fn may_be_subtype(
    from: DeclSite,
    to: Target<'_>,
    files: &Files<'_>,
    externals: &dyn Externals,
) -> bool {
    // Keyed on the site for the same reason `members_of`'s guard is: the same
    // span in two files is two declarations, and a walk that treated them as
    // one would stop short of a supertype it had not visited.
    let mut seen: Vec<DeclSite> = Vec::new();
    let mut queue = vec![from];
    while let Some(current) = queue.pop() {
        if matches!(to, Target::Local(to) if *to == current) {
            return true;
        }
        // A cycle is invalid Java and ordinary input; the guard drops the
        // repeat rather than truncating the walk, because everything the
        // repeat could reach has been queued already.
        if seen.contains(&current) {
            continue;
        }
        if seen.len() >= MAX_SUPERTYPES {
            return true;
        }
        // Each frame's supertypes are resolved in the frame's own file, the
        // same rule `members_of`'s walk follows: what `extends Base` means is
        // the writing file's question. A file the set cannot reach is as
        // opaque as a name that resolved to nothing, so it answers "could be".
        let Some(view) = files.viewing(&current.file) else {
            return true;
        };
        let Some(decl) = view.current_syntax().declaration_at(current.span) else {
            return true;
        };
        seen.push(current);
        // An anonymous class's supertype is the type it instantiates, which
        // `FileSyntax` records as its *name* and never as an `extends` — so the
        // walk cannot see it, and `f(this)` inside `new Marker() { … }` would
        // otherwise be excluded from `f(Marker)`. It is reachable as an
        // argument type through `this` alone; reading the name as a supertype is
        // the upgrade, and it belongs next to the same gap in `members_of`.
        if decl.kind == DeclKind::AnonymousClass {
            return true;
        }
        // Past its `extends` and `implements` a declaration has one supertype
        // nobody writes down, and only a class's and an interface's is
        // `java.lang.Object`: JLS 8.9, 8.10 and 9.6 give an enum
        // `java.lang.Enum`, a record `java.lang.Record` and an annotation type
        // `java.lang.annotation.Annotation`, and each of those brings its own
        // interfaces — an enum is `Comparable` and `Serializable` without
        // saying so. So the world is asked about that supertype by fqn, as a
        // written external one is, and a world that does not hold it answers
        // "could be". Only an external target has to ask: no implicit
        // supertype leads back into a file this query can reach, so a local
        // target is unaffected.
        if matches!(to, Target::External(_)) {
            let implicit = match decl.kind {
                DeclKind::Class | DeclKind::Interface => None,
                DeclKind::Enum => Some("java.lang.Enum"),
                DeclKind::Record => Some("java.lang.Record"),
                DeclKind::AnnotationType => Some("java.lang.annotation.Annotation"),
                _ => return true,
            };
            if implicit.is_some_and(|fqn| may_reach(fqn, to, externals)) {
                return true;
            }
        }
        for written in decl.extends.iter().chain(decl.implements.iter()) {
            let at = decl.name_span.start_byte;
            match resolve_written(written, &view, externals, at) {
                Type::Declared { decl, .. } => queue.push(decl),
                Type::External { fqn, .. } => {
                    if may_reach(&fqn, to, externals) {
                        return true;
                    }
                }
                // A supertype nothing resolves answers "could be": the name
                // could mean anything at all, including a subtype of the
                // target. Unchanged from Phase 2, and the reason the arm above
                // it had to be about resolution rather than about locality.
                _ => return true,
            }
        }
    }
    match to {
        Target::Local(_) => false,
        // The walk has seen the whole written closure and none of it reaches
        // `to`. What nobody writes is `extends Object`, so the implicit
        // supertype is asked here, once, rather than queued at every node.
        // Only of a `to` the platform holds: a dependency's or the
        // workspace's `to` may be this very declaration under its fqn,
        // reached through another file — `r.register(this)` against
        // `register(Foo)` — which `Object` says nothing about.
        Target::External(to) => {
            !known(to, externals) || externals.is_subtype(OBJECT, to) != Some(false)
        }
    }
}

/// Whether an external supertype the classpath resolved may reach `to`, which
/// is the walk's cue to stop and answer "could be".
///
/// The two targets are not symmetric. A local target cannot be reached through
/// an external supertype at all, so a *known* one contributes nothing and the
/// walk carries on past it, where an *unknown* one is as opaque as a name that
/// resolved to nothing and stops the walk. That is the whole distinction:
/// resolved versus unresolved, not external versus local.
///
/// ponytail: "cannot lead back into the open buffer" is the same JDK-only
/// assumption the `(External, Declared)` arm of [`cannot_pass`] rests on, and
/// it survives a workspace source index for the same reason that one does — a
/// workspace fqn is not `known`, so this walk treats it as opaque and stops
/// rather than stepping past it. Opaque is the refusing direction. The upgrade
/// is the same too, and it is half done: a fact carries where it came from now
/// — the `src.zip` entry on `javelin_classpath::TypeFacts` — but `Externals`
/// answers about types rather than about where they were read, so neither arm
/// can tell one provenance from another and both are kept honest by what they
/// are allowed to see instead.
fn may_reach(supertype: &str, to: Target<'_>, externals: &dyn Externals) -> bool {
    match to {
        Target::Local(_) => !known(supertype, externals),
        // `Some(true)` and `None` both stop the walk, for opposite reasons: the
        // target is genuinely reachable, or nothing here can show that it is
        // not. Only `Some(false)` lets the walk past.
        Target::External(to) => externals.is_subtype(supertype, to) != Some(false),
    }
}

/// Whether the world holds a type fact for `fqn` at all.
///
/// [`Externals`] offers no `contains`, deliberately — it answers questions
/// about types rather than about its own contents — but `is_subtype` is
/// reflexive on exactly the names it holds, so asking whether a type is its own
/// subtype asks whether the world has heard of it. Two lookups of one key on
/// the real world, which is what makes it affordable once per candidate.
fn known(fqn: &str, externals: &dyn Externals) -> bool {
    externals.is_subtype(fqn, fqn) == Some(true)
}

/// Whether `from` converts to `to` with no cast: identical, or one of JLS
/// 5.1.2's widening primitive conversions.
///
/// Not included, deliberately: the narrowing a *constant* expression gets in an
/// assignment context. `byte b = 1;` compiles, but JLS 5.3's method invocation
/// conversion does not carry that rule, so `f(byte)` at `f(1)` does not
/// compile and excluding it is sound.
fn widens(from: Primitive, to: Primitive) -> bool {
    if from == to {
        return true;
    }
    let wider: &[Primitive] = match from {
        Primitive::Byte => &[
            Primitive::Short,
            Primitive::Int,
            Primitive::Long,
            Primitive::Float,
            Primitive::Double,
        ],
        // `char` widens to `int` and up but not to `short`, and `short` not to
        // `char`: the two are the same width and neither range contains the
        // other.
        Primitive::Short | Primitive::Char => &[
            Primitive::Int,
            Primitive::Long,
            Primitive::Float,
            Primitive::Double,
        ],
        Primitive::Int => &[Primitive::Long, Primitive::Float, Primitive::Double],
        Primitive::Long => &[Primitive::Float, Primitive::Double],
        Primitive::Float => &[Primitive::Double],
        // `boolean` converts to nothing but itself, which the equality above
        // already answered.
        Primitive::Double | Primitive::Boolean => &[],
    };
    wider.contains(&to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoExternals;
    use crate::externals::ExternalMembers;
    use crate::files::FileId;
    use crate::infer::{Access, enclosing_type, sole_candidate};
    use crate::members::members_of;
    use javelin_document::{DocumentId, TextDocument};
    use javelin_parser::{
        Expr, FileSyntax, ImportDecl, JavaParser, ScopeTree, SyntaxTree, unqualified_call_at,
    };

    /// A stand-in classpath: a fully-qualified name and its direct supertypes,
    /// resolved by last segment and walked for subtyping. Hand-written like
    /// `resolve.rs`'s, and for the same reason — reading a `src.zip` or a
    /// `TypeWorld` here would test the classpath, and what is under test is the
    /// door.
    ///
    /// The three answers it gives match `TypeWorld`'s contract where it
    /// matters: a name absent from the list is a name this world has never
    /// heard of and answers `None` to everything, including about itself.
    struct Classpath {
        types: &'static [(&'static str, &'static [&'static str])],
    }

    /// Enough of `java.lang`, `java.io` and `java.time` for a box type, a
    /// reference type that is not one, an interface a file-local class can
    /// implement, and an enum.
    ///
    /// Every entry's supertype list is what `sources.rs` would actually emit
    /// for that type's source, which is the shape this fixture got wrong for
    /// a whole phase: an interface's source writes no `extends`, so the fact
    /// carries an *empty* list and never `java.lang.Object`. `Object` is a
    /// property of the world rather than of a fact, and [`Self::is_subtype`]
    /// answers it the same way `TypeWorld` does.
    ///
    /// `java.time.DayOfWeek` is the enum, and the reader marks an enum's
    /// closure incomplete because `java.lang.Enum` is never written down.
    /// This list has no `supertypes_complete` column, so incompleteness is
    /// spelled the only way it can be — a supertype naming a fact the world
    /// does not hold, which stops the walk at exactly the same `None`.
    const JDK: &[(&str, &[&str])] = &[
        ("java.lang.Object", &[]),
        ("java.io.Serializable", &[]),
        (
            "java.lang.Number",
            &["java.io.Serializable", "java.lang.Object"],
        ),
        ("java.lang.Integer", &["java.lang.Number"]),
        ("java.lang.Long", &["java.lang.Number"]),
        (
            "java.lang.String",
            &["java.io.Serializable", "java.lang.Object"],
        ),
        ("java.time.DayOfWeek", &["java.lang.Enum"]),
        ("java.util.List", &[]),
    ];

    /// The two boxes the tests below need. Not a table of `java.lang`: the real
    /// implementation's mapping lives with the classpath, and this one only has
    /// to be consistent with itself.
    const BOXES: &[(Primitive, &str)] = &[
        (Primitive::Int, "java.lang.Integer"),
        (Primitive::Long, "java.lang.Long"),
    ];

    impl Classpath {
        fn entry(&self, fqn: &str) -> Option<(&'static str, &'static [&'static str])> {
            self.types.iter().copied().find(|(name, _)| *name == fqn)
        }
    }

    impl Externals for Classpath {
        fn resolve(
            &self,
            name: &str,
            _package: Option<&str>,
            _imports: &[ImportDecl],
        ) -> Option<String> {
            let mut hits = self
                .types
                .iter()
                .filter(|(fqn, _)| fqn.rsplit('.').next() == Some(name));
            let (first, _) = *hits.next()?;
            hits.next().is_none().then(|| first.to_owned())
        }

        fn is_subtype(&self, from: &str, to: &str) -> Option<bool> {
            let (from, _) = self.entry(from)?;
            self.entry(to)?;
            // Answered before the walk, exactly as `TypeWorld` answers it and
            // for the trait's stated reason: nothing writes `extends Object`,
            // so no closure this world holds contains it to be found.
            if to == OBJECT {
                return Some(true);
            }
            let mut seen: Vec<&str> = Vec::new();
            let mut stack = vec![from];
            while let Some(current) = stack.pop() {
                if current == to {
                    return Some(true);
                }
                if seen.contains(&current) {
                    continue;
                }
                seen.push(current);
                // `?`, so a supertype this world does not hold answers "cannot
                // say" rather than "no" — the closure cannot be shown whole
                // past a name that is missing from it.
                let (_, supertypes) = self.entry(current)?;
                stack.extend(supertypes);
            }
            Some(false)
        }

        fn boxed(&self, primitive: Primitive) -> Option<String> {
            let (_, fqn) = BOXES.iter().find(|(kind, _)| *kind == primitive)?;
            self.entry(fqn).map(|(fqn, _)| fqn.to_owned())
        }

        fn unboxed(&self, fqn: &str) -> Option<Primitive> {
            BOXES
                .iter()
                .find(|(_, boxed)| *boxed == fqn)
                .map(|(kind, _)| *kind)
        }

        /// Nothing here is about members: what this module decides is whether
        /// an argument can be passed, and a member's *type* arrives already
        /// resolved either way. The pairs this file's `cannot_pass` needs for
        /// an external member's parameter type already exist, which
        /// `crates/semantic/src/infer.rs` demonstrates end to end.
        fn members(&self, _fqn: &str) -> Option<ExternalMembers> {
            None
        }
    }

    /// The file every single-file fixture below is, and the *current* file of
    /// the cross-file ones.
    const FILE: &str = "file:///A.java";

    /// The second file the cross-file tests reach into. Never the current
    /// file: the point of those tests is that the walk leaves `FILE`.
    const OTHER: &str = "file:///B.java";

    /// A file the set never holds, for the test that asks what an
    /// unreachable declaration excludes.
    const ABSENT: &str = "file:///Absent.java";

    struct Fixture {
        document: TextDocument,
        tree: SyntaxTree,
        syntax: FileSyntax,
        scopes: ScopeTree,
        source: String,
        externals: Box<dyn Externals>,
    }

    fn fixture(source: &str) -> Fixture {
        fixture_with(source, Box::new(NoExternals))
    }

    /// The same file against a classpath that holds [`JDK`].
    fn with_jdk(source: &str) -> Fixture {
        fixture_with(source, Box::new(Classpath { types: JDK }))
    }

    /// One fixture's views and document. The document is named [`FILE`]
    /// whichever file the views are going to stand for: nothing reads a
    /// document's id back off a `FileSyntax` or a `ScopeTree`, and the id
    /// that decides anything is the one the set is keyed on.
    fn fixture_with(source: &str, externals: Box<dyn Externals>) -> Fixture {
        // `replacen`, not `replace`: a fixture is free to contain a `|` of its
        // own, and silently eating it would mangle the Java under test.
        let source = source.replacen('|', "", 1);
        let document = TextDocument::new(DocumentId::new(FILE), 1, source.clone());
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        Fixture {
            syntax: FileSyntax::build(&document, &tree),
            scopes: ScopeTree::build(&document, &tree),
            document,
            tree,
            source,
            externals,
        }
    }

    impl Fixture {
        fn files(&self) -> Files<'_> {
            Files::single(FileId::new(FILE), &self.syntax, &self.scopes)
        }

        /// Resolve a type by name at the end of the file, where every top-level
        /// declaration is in scope.
        fn ty(&self, written: &str) -> Type {
            let at = self.source.len() as u32 - 1;
            resolve_written(written, &self.files(), &*self.externals, at)
        }

        fn cannot_pass(&self, from: &str, to: &str) -> bool {
            cannot_pass(
                &self.ty(from),
                &self.ty(to),
                &self.files(),
                &*self.externals,
            )
        }

        /// This fixture's own views as a one-file set under `file`, which is
        /// how a receiver sited in another file is built: `resolve_written`
        /// asks the current file and then [`Externals`] and deliberately
        /// never searches the set's other entries, so nothing in file A can
        /// name a type only file B declares. Resolved in a set of its own
        /// rather than through `Files::viewing`, so a test's setup does not
        /// depend on the very function the test is about.
        fn declared_as(&self, file: &str, written: &str) -> Type {
            let one = Files::single(FileId::new(file), &self.syntax, &self.scopes);
            resolve_written(
                written,
                &one,
                &*self.externals,
                self.source.len() as u32 - 1,
            )
        }
    }

    /// A set whose current file is [`FILE`] with `a`'s views, and which can
    /// also reach [`OTHER`] with `b`'s.
    fn two<'a>(a: &'a Fixture, b: &'a Fixture) -> Files<'a> {
        let mut set = Files::single(FileId::new(FILE), &a.syntax, &a.scopes);
        set.add(FileId::new(OTHER), &b.syntax, &b.scopes);
        set
    }

    /// The same type, re-sited into `file` with its span left alone.
    fn resited(ty: &Type, file: &str) -> Type {
        let Type::Declared {
            name,
            decl,
            arguments,
        } = ty
        else {
            panic!("expected a declared type, got {ty:?}");
        };
        Type::Declared {
            name: name.clone(),
            decl: DeclSite::new(FileId::new(file), decl.span),
            arguments: arguments.clone(),
        }
    }

    /// Which overload an unqualified call at `|` resolves to, named by its
    /// first parameter's written type.
    ///
    /// Through [`sole_candidate`] and a real parse rather than through
    /// `cannot_pass`: the arity filter, the elimination and the
    /// own-shadows-inherited check all sit between an argument's type and the
    /// member that wins, and a new fact is only worth anything if it survives
    /// all three.
    fn sole_overload(marked: &str, externals: Box<dyn Externals>) -> Option<String> {
        let at = marked.find('|').expect("mark the call with |") as u32;
        let f = fixture_with(marked, externals);
        let Expr::Call {
            name,
            name_span,
            arguments,
            ..
        } = unqualified_call_at(&f.document, &f.tree, at)?
        else {
            return None;
        };
        let owner = enclosing_type(&f.files(), name_span.start_byte);
        let found = sole_candidate(
            &owner,
            &name,
            Access::Call {
                arguments: &arguments,
            },
            &f.files(),
            &*f.externals,
        )?;
        found.first_parameter_type().map(str::to_owned)
    }

    #[test]
    fn a_primitive_passes_only_to_itself_or_something_wider() {
        let f = fixture("class A {}");
        for (from, to) in [
            ("int", "int"),
            ("int", "long"),
            ("int", "double"),
            ("byte", "short"),
            ("byte", "double"),
            ("char", "int"),
            ("long", "float"),
            ("float", "double"),
        ] {
            assert!(!f.cannot_pass(from, to), "{from} widens to {to}");
        }
        for (from, to) in [
            ("long", "int"),
            ("double", "float"),
            ("int", "byte"),
            ("int", "char"),
            ("char", "short"),
            ("short", "char"),
            ("boolean", "int"),
            ("int", "boolean"),
        ] {
            assert!(f.cannot_pass(from, to), "{from} does not widen to {to}");
        }
    }

    #[test]
    fn a_primitive_and_a_file_local_type_exclude_each_other() {
        // Boxing reaches only the JDK's supertypes, so neither direction is
        // possible — and the same holds for a file-local class that happens to
        // be *named* `Integer`, which is not `java.lang.Integer`.
        let f = fixture("class Foo {} class Integer {}");
        assert!(f.cannot_pass("int", "Foo"));
        assert!(f.cannot_pass("Foo", "int"));
        assert!(f.cannot_pass("int", "Integer"));
    }

    #[test]
    fn a_primitive_and_a_type_this_file_cannot_see_do_not_exclude_each_other() {
        // The Phase 3 statement, in its smallest form: `Integer` here is
        // unresolvable, and an unresolvable name could be `Object`, `Number` or
        // `Comparable`, all of which take an `int`.
        let f = fixture("class A {}");
        assert!(!f.cannot_pass("int", "Integer"));
        assert!(!f.cannot_pass("int", "String"));
    }

    #[test]
    fn null_excludes_a_primitive_and_nothing_else() {
        let f = fixture("class Foo {}");
        assert!(cannot_pass(
            &Type::Null,
            &f.ty("int"),
            &f.files(),
            &NoExternals
        ));
        assert!(!cannot_pass(
            &Type::Null,
            &f.ty("Foo"),
            &f.files(),
            &NoExternals
        ));
        assert!(!cannot_pass(
            &Type::Null,
            &f.ty("String"),
            &f.files(),
            &NoExternals
        ));
    }

    #[test]
    fn a_file_local_type_passes_to_a_supertype_this_file_declares_but_not_downwards() {
        let f = fixture("class Animal {} class Dog extends Animal {} class Cat extends Animal {}");
        assert!(!f.cannot_pass("Dog", "Animal"));
        assert!(!f.cannot_pass("Dog", "Dog"));
        assert!(f.cannot_pass("Animal", "Dog"));
        assert!(f.cannot_pass("Dog", "Cat"));
    }

    #[test]
    fn an_interface_this_file_declares_is_reached_through_implements() {
        let f = fixture("interface Named {} class Foo implements Named {} class Bar {}");
        assert!(!f.cannot_pass("Foo", "Named"));
        assert!(f.cannot_pass("Foo", "Bar"));
    }

    #[test]
    fn a_supertype_this_file_cannot_see_leaves_the_candidate_standing() {
        // The soundness case, and the reason the walk is conservative rather
        // than a plain reachability test: `Other` lives in another file, and
        // another file in this package may well implement `Marker`. Excluding
        // `Mid` here would pick the wrong overload rather than refuse.
        let f = fixture("interface Marker {} class Mid extends Other {}");
        assert!(!f.cannot_pass("Mid", "Marker"));
        // Not vacuous: the identical question about a class whose supertypes
        // this file *can* see is excluded.
        let seen = fixture("interface Marker {} class Base {} class Mid extends Base {}");
        assert!(seen.cannot_pass("Mid", "Marker"));
    }

    #[test]
    fn a_supertype_cycle_terminates_instead_of_hanging() {
        // Invalid Java, entirely valid input to a parser that never rejects.
        // Liveness first: this must return rather than hang. The answer it
        // returns is `true` — every link in the cycle resolves in this file, so
        // the walk sees the whole closure and `Marker` is not in it — which is
        // as meaningful as any answer about a hierarchy that cannot exist.
        let f = fixture("interface Marker {} class A extends B {} class B extends A {}");
        assert!(f.cannot_pass("A", "Marker"));
        assert!(!f.cannot_pass("A", "B"));
    }

    #[test]
    fn an_unresolvable_type_or_a_type_variable_excludes_nothing() {
        let f = fixture("class Foo {} class Box<T> { void m(T t) {} }");
        assert!(!f.cannot_pass("String", "Foo"));
        assert!(!f.cannot_pass("Foo", "String"));
        let variable = Type::Variable("T".into());
        assert!(!cannot_pass(
            &variable,
            &f.ty("Foo"),
            &f.files(),
            &NoExternals
        ));
        assert!(!cannot_pass(
            &f.ty("Foo"),
            &variable,
            &f.files(),
            &NoExternals
        ));
        assert!(!cannot_pass(
            &f.ty("int"),
            &variable,
            &f.files(),
            &NoExternals
        ));
    }

    #[test]
    fn an_array_is_separated_from_every_non_array_but_not_from_an_array() {
        let f = with_jdk("class Foo {} class Bar {}");
        assert!(!f.cannot_pass("Foo[]", "Bar[]"));
        assert!(f.cannot_pass("Foo[]", "Bar"));
        assert!(f.cannot_pass("Foo", "Bar[]"));
        assert!(f.cannot_pass("int[]", "int"));
        assert!(f.cannot_pass("int", "int[]"));
        assert!(f.cannot_pass("String", "char[]"));
        assert!(f.cannot_pass("int[]", "String"));
        for to in ["Object", "java.io.Serializable"] {
            assert!(!f.cannot_pass("int[]", to), "an array is a {to}");
        }
        assert!(!f.cannot_pass("null", "Foo[]"));
    }

    #[test]
    fn a_void_argument_excludes_every_parameter() {
        let f = fixture("class Foo {}");
        assert!(f.cannot_pass("void", "Foo"));
        assert!(f.cannot_pass("void", "int"));
        assert!(f.cannot_pass("void", "String"));
    }

    #[test]
    fn an_overload_pair_differing_only_in_a_jdk_parameter_type_now_separates() {
        // The largest thing Phase 2c refused, and the whole point of a
        // classpath: `int` boxes to `java.lang.Integer`, which the world says
        // is not a `java.lang.String`, so `f(String)` is eliminated and one
        // candidate is left standing.
        let source = "class A { int f(int i) { return 1; } int f(String s) { return 2; } \
                      void m() { f|(1); } }";
        assert_eq!(
            sole_overload(source, Box::new(Classpath { types: JDK })),
            Some("int".to_owned())
        );
    }

    #[test]
    fn an_overload_pair_the_world_does_not_know_still_refuses() {
        // The same file with no classpath behind it answers exactly what Phase
        // 2 answered. Nothing here got smarter; the world did.
        let source = "class A { int f(int i) { return 1; } int f(String s) { return 2; } \
                      void m() { f|(1); } }";
        assert!(sole_overload(source, Box::new(NoExternals)).is_none());
    }

    #[test]
    fn a_misspelled_import_resolves_to_nothing_by_last_segment() {
        // `Lst` resolves to nothing at all, which is not the same as resolving
        // to something the world can reason about: the candidate stands and the
        // call answers nothing. The test above is the same shape with the name
        // spelled right, so this is not vacuous.
        //
        // The name says *by last segment* because that is all this fixture
        // does — [`Classpath::resolve`] ignores its `imports` argument, so the
        // `import` line is scenery and the refusal comes from no entry in
        // [`JDK`] ending in `Lst`. What a real import table does with a
        // misspelling belongs to `javelin-classpath`, and is tested there.
        let source = "import java.util.Lst; \
                      class A { int f(int i) { return 1; } int f(Lst l) { return 2; } \
                      void m() { f|(1); } }";
        assert!(sole_overload(source, Box::new(Classpath { types: JDK })).is_none());
    }

    #[test]
    fn a_declared_type_with_a_jdk_supertype_is_now_separable() {
        // Phase 2c had to keep every candidate here: `Serializable` resolved to
        // nothing, so the walk aborted with "could be" and a domain type with
        // any supertype outside the file was separable from nothing. A resolved
        // external supertype is one the walk can complete past.
        let source = "class Foo implements Serializable {} class Bar {}";
        let f = with_jdk(source);
        assert!(f.cannot_pass("Foo", "Bar"));
        // Only the local target is decided by the walk completing: `Foo` really
        // is a `Serializable`, and claiming otherwise would be the unsound
        // direction.
        assert!(!f.cannot_pass("Foo", "Serializable"));
        assert!(!fixture(source).cannot_pass("Foo", "Bar"));
    }

    #[test]
    fn a_boxed_type_is_passable_to_its_own_primitive_and_nothing_wider_downward() {
        // JLS 5.1.8 then 5.1.2: unbox, then widen. The chain runs in one
        // direction only, and a reference type that unboxes to nothing reaches
        // no primitive at all.
        let f = with_jdk("class A {}");
        assert!(!f.cannot_pass("Integer", "int"));
        assert!(!f.cannot_pass("Integer", "long"));
        assert!(!f.cannot_pass("Integer", "double"));
        assert!(f.cannot_pass("Integer", "short"));
        assert!(f.cannot_pass("Long", "int"));
        assert!(f.cannot_pass("String", "int"));
        // Boxing the other way is a single step and reaches the box type's own
        // supertypes, so it excludes neither `Integer` nor anything above it.
        assert!(!f.cannot_pass("int", "Integer"));
        assert!(!f.cannot_pass("int", "Number"));
        assert!(f.cannot_pass("int", "String"));
        // No world, no facts, no exclusions — `Integer` here is an unresolvable
        // name again and could mean anything.
        assert!(!fixture("class A {}").cannot_pass("Integer", "int"));
    }

    #[test]
    fn an_object_parameter_excludes_no_argument_at_all() {
        // Every one of these is assignable to `java.lang.Object`, and nobody
        // writes the `extends` that says so. Getting this wrong would exclude
        // every argument from the most common parameter type in Java.
        let f = with_jdk(
            "class Foo {} class Bar extends Foo {} \
             interface Marker {} class Impl implements Marker {}",
        );
        for from in [
            "Foo", "Bar", "Marker", "Impl", "Foo[]", "int", "long", "double", "String", "Integer",
        ] {
            assert!(!f.cannot_pass(from, "Object"), "{from} is an Object");
        }
        // Not vacuous: the same walk against a target that is not every type's
        // supertype does exclude.
        assert!(f.cannot_pass("Foo", "String"));
        assert!(f.cannot_pass("Impl", "Number"));
    }

    #[test]
    fn an_enum_or_a_record_is_not_excluded_from_what_its_implicit_supertype_brings() {
        // An enum is `Comparable` and `Serializable` through `java.lang.Enum`,
        // with none of it written down, and this world holds no
        // `java.lang.Enum` for the walk to ask. Excluding on the written
        // closure alone there would be a wrong pick.
        let f = with_jdk("enum Colour { RED } record Point(int x) {} class Plain {}");
        assert!(!f.cannot_pass("Colour", "Serializable"));
        assert!(!f.cannot_pass("Point", "Serializable"));
        assert!(!f.cannot_pass("Colour", "Object"));
        // Not vacuous: a plain class's implicit supertype *is* `Object`, and
        // the same question about it is answered.
        assert!(f.cannot_pass("Plain", "Serializable"));
    }

    #[test]
    fn an_enum_or_a_record_is_separated_through_the_implicit_supertype_the_world_holds() {
        const WORLD: &[(&str, &[&str])] = &[
            ("java.lang.Object", &[]),
            ("java.io.Serializable", &[]),
            ("java.lang.Comparable", &[]),
            (
                "java.lang.Enum",
                &["java.lang.Comparable", "java.io.Serializable"],
            ),
            ("java.lang.Record", &[]),
            ("java.lang.annotation.Annotation", &[]),
            ("java.lang.Number", &["java.io.Serializable"]),
        ];
        let f = fixture_with(
            "enum Colour { RED } record Point(int x) {} @interface Tag {}",
            Box::new(Classpath { types: WORLD }),
        );
        assert!(f.cannot_pass("Colour", "Number"));
        assert!(f.cannot_pass("Point", "Number"));
        assert!(f.cannot_pass("Tag", "Number"));
        // Not vacuous: what the implicit supertype brings still passes.
        assert!(!f.cannot_pass("Colour", "Serializable"));
        assert!(!f.cannot_pass("Colour", "Comparable"));
        assert!(!f.cannot_pass("Point", "Object"));
    }

    #[test]
    fn an_external_interface_is_passable_to_object() {
        // The argument side, which no check reached for a whole phase, and the
        // one that costs the most: an interface's fact carries no
        // `java.lang.Object`, so a closure walk that believes itself complete
        // drains without finding it. Excluding `f(Object)` here hands the call
        // to whichever sibling overload the buffer cannot resolve — a wrong
        // jump, in a shape every real file has.
        let f = with_jdk("class A {}");
        assert!(!f.cannot_pass("Serializable", "Object"));
        // Not vacuous: the same fact still separates the pair in the direction
        // a complete closure answers.
        assert!(f.cannot_pass("Serializable", "Number"));
    }

    #[test]
    fn an_external_enum_cannot_say_and_so_excludes_nothing() {
        // The external half of the ceiling
        // `an_enum_or_a_record_is_not_excluded_from_what_its_implicit_supertype_brings`
        // records for a local declaration. `java.lang.Enum` is not in the
        // source for a walk to follow, and it brings `Comparable` and
        // `Serializable` with it, so the closure is not whole and nothing may
        // be excluded on having drained it.
        let f = with_jdk("class A {}");
        assert!(!f.cannot_pass("DayOfWeek", "Number"));
        assert!(!f.cannot_pass("DayOfWeek", "Serializable"));
        // `Object` is the one answer that does not need the closure.
        assert!(!f.cannot_pass("DayOfWeek", "Object"));
        // Not vacuous, and the refusal runs one way only: it is the
        // *argument's* closure a walk drains, so the same enum in the
        // parameter position is separable like any other type.
        assert!(f.cannot_pass("String", "DayOfWeek"));
        assert!(f.cannot_pass("String", "Number"));
    }

    /// JLS 15.12.2.5 where it is provable: the one candidate every argument
    /// reaches without boxing, whose parameters are each a subtype of every
    /// other survivor's.
    #[test]
    fn the_most_specific_applicable_overload_is_picked() {
        let jdk = || Box::new(Classpath { types: JDK }) as Box<dyn Externals>;
        for (source, picked) in [
            (
                "void f(int i) {} void f(long l) {} void m() { f|(1); }",
                "int",
            ),
            (
                "void f(String s) {} void f(Object o) {} void m() { f|(null); }",
                "String",
            ),
            (
                "void f(Object o) {} void f(String s) {} void m(String s) { f|(s); }",
                "String",
            ),
            (
                "void f(char[] c) {} void f(String s) {} void f(Object o) {} \
                 void m(String s) { f|(s); }",
                "String",
            ),
            (
                "void f(Animal a) {} void f(Dog d) {} void m() { f|(new Dog()); }",
                "Dog",
            ),
            (
                "void f(Number n, Dog d) {} void f(Object o, Animal a) {} \
                 void m(Integer i) { f|(i, new Dog()); }",
                "Number",
            ),
        ] {
            let source =
                format!("class Animal {{}} class Dog extends Animal {{}} class A {{ {source} }}");
            assert_eq!(
                sole_overload(&source, jdk()).as_deref(),
                Some(picked),
                "{source}"
            );
        }
    }

    /// What is not provable picks nothing: boxing (the second phase), a
    /// survivor neither side of which is more specific, an argument of
    /// unknown type, a generic or variadic survivor, and a list the walk does
    /// not show whole.
    #[test]
    fn an_overload_set_specificity_cannot_settle_answers_nothing() {
        let jdk = || Box::new(Classpath { types: JDK }) as Box<dyn Externals>;
        for source in [
            "class A { void f(Integer i) {} void f(Object o) {} void m() { f|(1); } }",
            "class A { void f(Number n, String s) {} void f(Integer i, Object o) {} \
             void m(Integer i, String s) { f|(i, s); } }",
            "class A { void f(String s) {} void f(Object o) {} void m() { f|(unknown); } }",
            "class A { <T> void f(T t) {} void f(String s) {} void m(String s) { f|(s); } }",
            "class A extends Missing { void f(String s) {} void f(Object o) {} \
             void m(String s) { f|(s); } }",
            // A raw argument is not the parameterized parameter, and a
            // differently-parameterized one is not applicable at all.
            "class A { void f(List<String> l) {} void f(Object o) {} void m(List l) { f|(l); } }",
            "class A { void f(List<String> l) {} void f(Object o) {} \
             void m(List<Integer> l) { f|(l); } }",
            // `int[]` is no `long[]`: array covariance is for references.
            "class A { void f(long[] a) {} void f(Object o) {} void m(int[] a) { f|(a); } }",
            // `Object.wait(long, int)` is the most specific here, and no
            // walk of a local type holds it.
            "class A { void wait(long a, long b) {} void wait(double a, double b) {} \
             void m() { wait|(1L, 1); } }",
        ] {
            assert_eq!(sole_overload(source, jdk()), None, "{source}");
        }
    }

    /// JLS 15.12.2.1 drops a member the call cannot see before specificity
    /// is ranked, so a `private` winner of another type is not Java's pick:
    /// `f(Object)` is. Refusing is the safe half of that.
    #[test]
    fn a_most_specific_winner_the_caller_may_not_see_answers_nothing() {
        let jdk = || Box::new(Classpath { types: JDK }) as Box<dyn Externals>;
        let hidden = "class Base { private void f(String s) {} void f(Object o) {} } \
                      class A extends Base { void m(String s) { f|(s); } }";
        assert_eq!(sole_overload(hidden, jdk()), None);
        let seen = "class Base { void f(String s) {} void f(Object o) {} } \
                    class A extends Base { void m(String s) { f|(s); } }";
        assert_eq!(sole_overload(seen, jdk()).as_deref(), Some("String"));
    }

    /// An array written either way is an array: `String... args` and
    /// `String a[]` are no `String`.
    #[test]
    fn a_variadic_or_c_style_array_argument_is_typed_as_an_array() {
        let jdk = || Box::new(Classpath { types: JDK }) as Box<dyn Externals>;
        for (source, picked) in [
            (
                "class A { void f(String s) {} void f(Object o) {} void m(String... a) { f|(a); } }",
                Some("Object"),
            ),
            (
                "class A { void f(String s) {} void f(Object o) {} void m(String a[]) { f|(a); } }",
                Some("Object"),
            ),
            (
                "class A { void f(String a[]) {} void f(Object o) {} void m(String[] a) { f|(a); } }",
                Some("String[]"),
            ),
        ] {
            assert_eq!(sole_overload(source, jdk()).as_deref(), picked, "{source}");
        }
    }

    /// Java's pick is the `valueOf(String)` every enum has and nobody
    /// writes, more specific than either written overload.
    #[test]
    fn an_enums_unwritten_members_keep_its_overloads_unranked() {
        let jdk = || Box::new(Classpath { types: JDK }) as Box<dyn Externals>;
        let written = "static E valueOf(Serializable s) { return null; } \
                       static E valueOf(Object o) { return null; } \
                       void m(String s) { valueOf|(s); }";
        assert_eq!(
            sole_overload(&format!("enum E {{ A; {written} }}"), jdk()),
            None
        );
        assert_eq!(
            sole_overload(&format!("class E {{ {written} }}"), jdk()).as_deref(),
            Some("Serializable"),
            "a class has no such member"
        );
    }

    #[test]
    fn f_integer_against_f_int_at_f_one_answers_nothing() {
        // The pair the new boxing fact could most easily be mistaken for having
        // closed: `1` is applicable to `int` directly and to `Integer` by
        // boxing, so both stand. Java prefers the unboxed one by phase
        // ordering, which is 15.12.2's ranking rather than an elimination.
        let source = "class A { int f(Integer i) { return 1; } int f(int j) { return 2; } \
                      void m() { f|(1); } }";
        assert!(sole_overload(source, Box::new(Classpath { types: JDK })).is_none());
    }

    #[test]
    fn a_supertype_in_another_file_completes_the_subtype_walk() {
        // The `members_of` equivalent is
        // `a_supertype_in_another_file_contributes_its_members`; this is the
        // same crossing in the walk that *excludes*. Both files declare a
        // `Marker`, so the frame's `implements Marker` has two possible
        // meanings and only B's is right — the walk is in B's `Impl`, and B's
        // `implements` clause is written against B's own scopes.
        //
        // Resolving it in the current file instead would answer nothing
        // rather than wrongly for the reachable target below, but for the
        // unreachable one it abandons the walk at an unresolved name and so
        // stops excluding a pair it can separate.
        let a = fixture("interface Marker {} class A {}");
        let b = fixture("interface Marker {} class Impl implements Marker {} class Other {}");
        let files = two(&a, &b);
        let impl_in_b = b.declared_as(OTHER, "Impl");

        // Reachable through the crossing: `Impl` really is B's `Marker`.
        assert!(!cannot_pass(
            &impl_in_b,
            &b.declared_as(OTHER, "Marker"),
            &files,
            &NoExternals
        ));
        // And the walk *completes* in B rather than giving up in it, which is
        // what lets an unrelated target be excluded at all.
        assert!(cannot_pass(
            &impl_in_b,
            &b.declared_as(OTHER, "Other"),
            &files,
            &NoExternals
        ));
    }

    #[test]
    fn a_file_the_set_does_not_hold_excludes_nothing() {
        // Same span, said of a file nothing can reach. Falling back to the
        // current file would walk `Impl`'s real hierarchy, find `Other` is
        // not in it, and exclude — an answer about a declaration this
        // argument was never claimed to be.
        let f = fixture("interface Marker {} class Impl implements Marker {} class Other {}");
        let impl_in_a = f.declared_as(FILE, "Impl");
        let other = f.declared_as(FILE, "Other");

        assert!(!cannot_pass(
            &resited(&impl_in_a, ABSENT),
            &other,
            &f.files(),
            &NoExternals
        ));
        // Not vacuous: the identical span in the file the set does hold is
        // excluded.
        assert!(cannot_pass(&impl_in_a, &other, &f.files(), &NoExternals));
    }

    #[test]
    fn a_candidate_whose_file_the_set_cannot_reach_hides_nothing_and_is_hidden_by_nothing() {
        // The one direction where a missing parameter list would be a *pick*
        // rather than a refusal: both these methods take nothing, so a
        // candidate with no parameter types to compare would match each of
        // them and shadow the whole set.
        let f = fixture("class A { void f() {} void g() {} }");
        let a = f.declared_as(FILE, "A");
        let matches = members_of(&a, &f.files(), &*f.externals, false);
        let own = matches.first().expect("A declares members").clone();

        // Not vacuous: in the file the set holds, this candidate really does
        // hide its zero-parameter sibling.
        assert!(hides_the_rest(&own, &matches, &f.files(), &*f.externals));

        let orphan = own.resited(FileId::new(ABSENT));
        assert!(!hides_the_rest(
            &orphan,
            &matches,
            &f.files(),
            &*f.externals
        ));
        assert!(!hides_the_rest(
            &own,
            std::slice::from_ref(&orphan),
            &f.files(),
            &*f.externals
        ));
    }
}
