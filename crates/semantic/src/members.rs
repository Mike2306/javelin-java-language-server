//! What a type exposes to a `.`, walking the supertypes the files this query
//! can reach declare.
//!
//! Not "this file" any more: a frame carries the file its declaration was
//! read out of, and each frame's supertypes are resolved against *that*
//! file's package, imports and scopes rather than the file the question was
//! asked in.
//!
//! An external *receiver* is answered too, and not by this walk: a
//! [`Type::External`] names one classpath type, and the members of one are the
//! world's to report through [`Externals::members`], already closed over its
//! supertypes. So `members_of` has two halves — a walk over declarations, and
//! a question about facts — and a [`Member`] says which of the two it came
//! from.
//!
//! The halves meet at one place, and only there: a supertype the walk resolves
//! to a [`Type::External`] is *asked* rather than skipped, so
//! `class Foo extends ArrayList` inherits what a receiver of `ArrayList`
//! inherits. What crosses is a member list and never a frame — the world's
//! answer is already closed, so there is nothing below it for the walk to
//! visit, and both halves are charged against one budget and one cycle guard
//! each; see [`members_of`].
//!
//! One limit is left, and it is not the addressing. Reaching a second *file*
//! needs a [`DeclSite`] that already names it — the receiver's, or one a type
//! argument's substitution carried in — because `resolve_written` asks the
//! current file and then the classpath and never the set's other entries:
//! which file a *written* name means is a package-and-import question, and
//! answering it needs a workspace index.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use javelin_parser::{Decl, DeclKind};

use crate::externals::{ExternalMember, ExternalType, Externals, last_segment, rendered};
use crate::files::{DeclSite, FileId, Files};
use crate::resolve::resolve_written;
use crate::types::{Type, split_arguments};

/// How many supertypes the walk may spend in total, across both breadth and
/// depth — not "how deep", since a wide hierarchy spends the budget just as
/// fast as a tall one. The cycle guard already terminates a cycle; this
/// bounds a long but acyclic chain instead.
///
/// One budget over both halves: a declaration the walk visits and an external
/// closure it asks for both cost one, so a class implementing a hundred
/// framework interfaces is bounded by the same number a hundred file-local
/// ones are. Two budgets would let the cheaper half be spent to the limit and
/// then the other, which is a limit of `2 × MAX_SUPERTYPES` written as if it
/// were one — and the external half is the wide one in a real project.
///
/// ponytail: hitting this truncates the member list, and half of what was
/// owed here has been paid: exhaustion lowers `walk_members`' completeness
/// flag, so a completion list over it is returned `isIncomplete`. What is
/// still owed is the *louder* telling — an `Answer::Partial` on the wire and a
/// `tracing` event — because a flag says "there may be more" and neither says
/// which sixty-fifth supertype went missing. It was a backstop while depth was
/// the only way to spend it, because a real file's supertype chain is nowhere
/// near 64 declarations deep. Sharing the budget across the two halves makes
/// breadth the binding case instead, and ordinary Java can reach it: a class
/// implementing sixty framework interfaces spends sixty of these, and what
/// then goes missing is the *local* members that were all present a phase ago
/// — a frame still in the queue when the budget runs out is dropped. The
/// budget stays one, which is the shape this crate wants.
///
/// Also unmodelled: a diamond that reaches the same supertype twice with
/// different type arguments. `seen` is keyed by declaration, so the first pop
/// wins and the second instantiation's arguments are dropped. Unchanged by
/// the walk reaching more than one file, since the key is a whole
/// [`DeclSite`] either way; would need per-instantiation keys to fix.
const MAX_SUPERTYPES: usize = 64;

/// How many type nodes one substitution may produce.
///
/// ponytail: a flat budget rather than shared structure. A chain of
/// `class A0<T> extends A1<Pair<T, T>>` doubles its argument at every level,
/// so a level cap bounds depth and not size. The upgrade is to intern types,
/// so a repeated argument costs one node and this budget can go.
const MAX_TYPE_NODES: u32 = 4096;

/// One member a type exposes.
///
/// The member's *type* is not here: see [`member_type`], which resolves one on
/// demand. What is here instead is the substitution that was in force in the
/// frame the member came from, which is the only part of that answer the walk
/// knows and the caller could not work out for itself. `Rc` so cloning a
/// `Member` stays a pointer bump however many members a frame contributed.
///
/// Where the member came from is a `MemberSite` rather than three fields,
/// and everything a caller can ask is an accessor below. What that buys is
/// that each question is answered in one place rather than once per caller —
/// and, since a declaration and a fact answer several of them differently,
/// that no caller can ask one of a member that has no answer to give.
#[derive(Debug, Clone)]
pub struct Member<'a> {
    site: MemberSite<'a>,
    /// Only meaningful as an input to [`member_type`], and read nowhere but
    /// this module today.
    ///
    /// `pub(crate)` says where the boundary that matters is rather than where
    /// the reads are: a substitution belongs to the frame the walk was in, so
    /// nothing outside this crate has any business supplying or reading one.
    /// It is not what keeps a substitution from arriving from elsewhere — the
    /// private site is, since [`members_of`] is then the only thing that can
    /// construct a `Member` at all.
    pub(crate) subs: Rc<HashMap<String, Type>>,
    /// For a fact, the external type whose closure it came from — the
    /// receiver's, or the external supertype a file-local frame wrote — so a
    /// member can tell whether that type declares it itself. `None` for a
    /// declaration.
    closure: Option<Rc<str>>,
    /// For a fact a source generator wrote into a file-local type — see
    /// [`Externals::generated_members`] — the declaration it was written
    /// into and that declaration's file, so the fact is that type's own.
    generated_into: Option<(&'a Decl, FileId)>,
}

/// Where a member was read from, which is what bounds how much of it can be
/// answered.
///
/// Two variants, and the split is what every accessor below is a decision
/// about: a declaration has a file, a span, modifiers and a doc comment, and a
/// fact has none of those. Being an enum is what makes each of those decisions
/// one the compiler insists on rather than one a reader has to go looking for.
#[derive(Debug, Clone)]
enum MemberSite<'a> {
    /// Borrowed out of a `FileSyntax` one of the query's files holds: the
    /// member's own declaration, the declaration that declares it — which is
    /// not always the type asked about — and the file both were read out of,
    /// which is not always the file the question was asked in.
    Local {
        decl: &'a Decl,
        owner: &'a Decl,
        file: FileId,
    },
    /// A fact the world holds about a type no file this query can reach
    /// declares. Owned rather than borrowed, because `Externals::members`
    /// builds its answer rather than lending it, and behind an `Arc` so that
    /// cloning a `Member` stays a pointer bump — completion clones one per
    /// candidate, and the list is thousands long on a god-class — and so the
    /// world's list is shared rather than copied into it.
    External(Arc<ExternalMember>),
}

impl<'a> Member<'a> {
    pub fn name(&self) -> &str {
        match &self.site {
            MemberSite::Local { decl, .. } => &decl.name,
            MemberSite::External(facts) => &facts.name,
        }
    }

    pub fn kind(&self) -> DeclKind {
        match &self.site {
            MemberSite::Local { decl, .. } => decl.kind,
            MemberSite::External(facts) => facts.kind,
        }
    }

    /// The member's documentation comment, and `None` for a member no file
    /// declares: a type fact carries a member's name, kind and types and never
    /// its comment, so there is no Javadoc for an external member to lose.
    pub fn doc(&self) -> Option<&str> {
        match &self.site {
            MemberSite::Local { decl, .. } => decl.doc.as_deref(),
            MemberSite::External(_) => None,
        }
    }

    /// The member's type as *written* — a field's type, a method's return
    /// type — for a caller that renders it rather than reasons about it.
    /// Resolving one is [`member_type`].
    ///
    /// `None` for an external member, and not for want of a type: a fact
    /// carries a *resolved* one and never the text, so there is nothing
    /// written to hand back. `ExternalMember::signature` is what renders one,
    /// and [`Member::external`] is how a caller reaches it.
    pub fn declared_type(&self) -> Option<&str> {
        match &self.site {
            MemberSite::Local { decl, .. } => decl.declared_type.as_deref(),
            MemberSite::External(_) => None,
        }
    }

    /// The parameter types as written, for a member a file declares.
    pub(crate) fn written_parameters(&self) -> Vec<&str> {
        match &self.site {
            MemberSite::Local { decl, .. } => decl
                .parameters
                .iter()
                .filter_map(|parameter| parameter.declared_type.as_deref())
                .collect(),
            MemberSite::External(_) => Vec::new(),
        }
    }

    /// The file the member was read out of, and `None` for a member no file
    /// declares.
    ///
    /// The `Option` is the point rather than an inconvenience: a caller that
    /// renders a span or slices a document has to compare this against the
    /// file it was handed, and a member with no file compares equal to nothing
    /// and is refused by the comparison the caller already makes.
    pub fn file(&self) -> Option<&FileId> {
        match &self.site {
            MemberSite::Local { file, .. } => Some(file),
            MemberSite::External(_) => None,
        }
    }

    /// The declaration behind a member a reachable file declares.
    ///
    /// The escape hatch for the two definition queries, which answer with
    /// spans and a slice of the requested document and so have nothing to say
    /// about a member that has neither. It is the only way a `Decl` field is
    /// readable outside this module, and deliberately the only one: a caller
    /// reaching through it is a caller assuming the member is local, and the
    /// `None` is where that assumption gets paid for instead of discovered.
    pub fn local_decl(&self) -> Option<&'a Decl> {
        match &self.site {
            MemberSite::Local { decl, .. } => Some(*decl),
            MemberSite::External(_) => None,
        }
    }

    /// The declaration that declares a local member, which is not always the
    /// type asked about; `None` for a fact.
    pub(crate) fn local_owner(&self) -> Option<&'a Decl> {
        match &self.site {
            MemberSite::Local { owner, .. } => Some(*owner),
            MemberSite::External(_) => None,
        }
    }

    /// The facts behind a member no file declares, and the mirror image of
    /// [`Member::local_decl`]: the one way anything outside this module reads
    /// a fact, and `None` where the member is a declaration instead.
    ///
    /// What a caller wants off one is a rendering — `ExternalMember`'s own
    /// `signature` — or the owner it was inherited from. Everything a caller
    /// could ask of *either* kind of member is an accessor above.
    pub fn external(&self) -> Option<&ExternalMember> {
        match &self.site {
            MemberSite::Local { .. } => None,
            MemberSite::External(facts) => Some(facts),
        }
    }

    /// How many formal parameters the member declares. Zero for anything that
    /// is not a method, which is what the arity filter reads it as.
    pub fn parameter_count(&self) -> usize {
        match &self.site {
            MemberSite::Local { decl, .. } => decl.parameters.len(),
            MemberSite::External(facts) => facts.parameters.len(),
        }
    }

    /// Whether the last of those parameters is a `...`. Java only permits
    /// varargs in last position, so this is the precise test and "any
    /// parameter is variadic" was not.
    pub fn is_variadic(&self) -> bool {
        match &self.site {
            MemberSite::Local { decl, .. } => decl
                .parameters
                .last()
                .is_some_and(|parameter| parameter.varargs),
            MemberSite::External(facts) => facts.variadic,
        }
    }

    /// Whether the member answers to a type name rather than a value.
    pub(crate) fn is_static(&self) -> bool {
        match &self.site {
            MemberSite::Local { decl, owner, .. } => is_static(decl, owner),
            MemberSite::External(facts) => facts.is_static,
        }
    }

    /// Whether the member is a method declaring type variables of its own.
    pub(crate) fn is_generic_method(&self) -> bool {
        match &self.site {
            MemberSite::Local { decl, .. } => decl.type_parameters.is_some(),
            MemberSite::External(facts) => !facts.type_parameters.is_empty(),
        }
    }

    /// Whether `owner` is the type this member is declared *on*, rather than
    /// one it was inherited into.
    ///
    /// The own-shadows-inherited rule turns on this and nothing else, so how
    /// the comparison is made lives here rather than at the rule. Each kind of
    /// member is compared the one way it can be identified — a declaration by
    /// the site it hangs off, a fact by the fully-qualified name it names as
    /// its owner — and a mismatched pair is not a demonstration of anything.
    /// [`members_of`] settles the same question about each *local* frame it is
    /// about to read members off, through the same `owned_by` — a frame the
    /// walk reads as inherited whose members the rule reads as own is one
    /// question answered two ways.
    ///
    /// The mismatched pair is the interesting one now that a file-local type's
    /// member list holds facts as well as declarations, and `false` is the
    /// answer in the direction that arises: a fact reached through a local
    /// subtype's `extends` was declared on the supertype by construction, so it
    /// is inherited and can never be own. That is what keeps
    /// `infer::sole_candidate`'s rule sound across the seam without a second
    /// comparison — the local override still *hides* the fact, because
    /// `apply::hides_the_rest` compares resolved parameter types and a fact
    /// has those. The rule can only ever narrow to a declaration the file
    /// holds, which is the direction with a span to answer with.
    pub(crate) fn declared_by(&self, owner: &Type) -> bool {
        match (&self.site, owner) {
            (
                MemberSite::Local {
                    owner: declaring,
                    file,
                    ..
                },
                Type::Declared { decl: site, .. },
            ) => owned_by(file, declaring, site),
            (MemberSite::External(facts), Type::External { fqn, .. }) => facts.owner == *fqn,
            // A fact a generator wrote into the declaration asked about is
            // that declaration's own, as its written members are.
            (MemberSite::External(_), Type::Declared { decl: site, .. }) => self
                .generated_into
                .as_ref()
                .is_some_and(|(owner, file)| owned_by(file, owner, site)),
            _ => false,
        }
    }

    /// A method with no body: what a functional interface has exactly one
    /// of. A declaration says so by `abstract`, or by being an interface's
    /// method with none of the three keywords a body needs.
    pub(crate) fn is_abstract(&self) -> bool {
        match &self.site {
            MemberSite::Local { decl, owner, .. } => {
                let has = |keyword: &str| decl.modifiers.iter().any(|modifier| modifier == keyword);
                decl.kind == DeclKind::Method
                    && (has("abstract")
                        || (owner.kind == DeclKind::Interface
                            && !has("default")
                            && !has("static")
                            && !has("private")))
            }
            MemberSite::External(facts) => facts.is_abstract,
        }
    }

    /// Whether the file the query is in can certainly call this member: a
    /// declaration in that same file that is not `private` — same file, same
    /// package — or a fact the world records as `public`.
    ///
    /// ponytail: a `private` declaration is never certain, though a call from
    /// inside its own top-level type may see it. The upgrade is the call's
    /// enclosing top-level type, which the pick does not carry.
    pub(crate) fn visible_from(&self, files: &Files<'_>) -> bool {
        match &self.site {
            MemberSite::Local { decl, file, .. } => {
                file == files.current() && !decl.modifiers.iter().any(|it| it == "private")
            }
            MemberSite::External(facts) => facts.is_public,
        }
    }

    /// The external type whose closure this fact came from, when that type
    /// declares the member itself rather than inheriting it — `ArrayList`
    /// for `ArrayList.get`, and nothing for the `List.get` it overrides.
    pub(crate) fn nearest_closure(&self) -> Option<&str> {
        match (&self.site, &self.closure) {
            (MemberSite::External(facts), Some(closure)) if facts.owner == **closure => {
                Some(closure)
            }
            _ => None,
        }
    }

    /// The external type whose closure this fact came from; `None` for a
    /// declaration.
    pub(crate) fn closure(&self) -> Option<&str> {
        self.closure.as_deref()
    }

    /// Whether this and `other` are the same member declaration, which
    /// `apply::hides_the_rest` needs because it compares a candidate against
    /// every survivor including itself, and a member does not hide itself.
    ///
    /// Two facts are the same member when they are the same fact, which is
    /// what `Arc::ptr_eq` asks: an owner and a name do not identify one, since
    /// two overloads share both. One member list is built once, and every
    /// `Member` a filter carries forward is a clone of an entry in it, so the
    /// pointers answer. Two separately built lists would answer `false` here
    /// and fall through to the parameter comparison at the call site, which is
    /// a refusal rather than a wrong pick.
    pub(crate) fn same_declaration(&self, other: &Member<'_>) -> bool {
        match (&self.site, &other.site) {
            (
                MemberSite::Local { decl, file, .. },
                MemberSite::Local {
                    decl: theirs,
                    file: their_file,
                    ..
                },
            ) => file == their_file && decl.name_span == theirs.name_span,
            (MemberSite::External(mine), MemberSite::External(theirs)) => Arc::ptr_eq(mine, theirs),
            _ => false,
        }
    }

    /// The member's parameter types, each resolved where it was written — or
    /// `None` when the file it was written in is not one this query can reach.
    /// An external member always answers `Some`: a fact carries its parameter
    /// types resolved already, so there is no file for it to be missing.
    ///
    /// `Option` rather than an empty list, because the two answers are not
    /// interchangeable and the difference is a wrong pick. An empty list
    /// excludes nothing in `apply::excluded_by_arguments`, where an empty
    /// `zip` is no comparison at all, but in `apply::hides_the_rest` it
    /// compares equal to every zero-parameter sibling and so would make the
    /// candidate *hide* all of them — a refusal turned into a pick. Each
    /// caller has to say which it means, and neither can get the dangerous one
    /// by default.
    ///
    /// ponytail: resolved without the owner's substitution, so a parameter
    /// declared `T` stays a `Type::Variable`. For exclusion that is merely
    /// unnarrowing — an unknown excludes nothing — but for
    /// `apply::hides_the_rest` it means a generic override whose type
    /// parameter the subtype pinned down (`class Base<T> { void m(T t) }`
    /// against `class A extends Base<Foo> { void m(Foo f) }`) reads as two
    /// different parameter types and so refuses rather than shadowing. The
    /// upgrade is to run `substitute_member` over a parameter's type as this
    /// module already does over a member's own, which is what `subs` is here
    /// for.
    /// Whether the type a declaration writes for parameter `index` may mean
    /// one `resolve_written`'s tiers do not see: a member type the file's
    /// types inherit, or one a static import brings in. `Entry` in a class
    /// implementing `Map` is `Map.Entry`, whatever an on-demand import holds.
    /// A fact's types are read exactly, so never.
    ///
    /// ponytail: asked only of a parameter resolved to a dependency's type,
    /// which a JDK argument excludes since Tier 5's slice 3. One resolved to
    /// the JDK's — `Error` beside an inherited `Base.Error` — still excludes
    /// as it did before; gating it too costs every local overload set a
    /// closure the world cannot show whole.
    pub(crate) fn parameter_may_be_shadowed(
        &self,
        index: usize,
        files: &Files<'_>,
        externals: &dyn Externals,
    ) -> bool {
        let MemberSite::Local { decl, file, .. } = &self.site else {
            return false;
        };
        let (Some(syntax), Some(written)) = (
            files.syntax(file),
            decl.parameters
                .get(index)
                .and_then(|parameter| parameter.declared_type.as_deref()),
        ) else {
            return true;
        };
        let first = written
            .split(['.', '<', '['])
            .next()
            .unwrap_or(written)
            .trim();
        externals.may_shadow_type(first, syntax)
    }

    pub(crate) fn parameter_types(
        &self,
        files: &Files<'_>,
        externals: &dyn Externals,
    ) -> Option<Vec<Type>> {
        match &self.site {
            MemberSite::Local { decl, file, .. } => {
                // In the member's own file, not the caller's: a parameter's
                // type is written where the method is, so that file's package,
                // imports and scopes are what settles it.
                let view = files.viewing(file)?;
                Some(
                    decl.parameters
                        .iter()
                        .map(|parameter| match &parameter.declared_type {
                            Some(written) => resolve_written(
                                written,
                                &view,
                                externals,
                                parameter.name_span.start_byte,
                            ),
                            // The answer `resolved_type` gives a member with
                            // nothing written: honest rather than wrong. Two
                            // of them are equal only when the parameters share
                            // a name, which is as close to a demonstration as
                            // a buffer this broken offers.
                            None => Type::Unknown(parameter.name.clone()),
                        })
                        .collect(),
                )
            }
            MemberSite::External(facts) => Some(
                facts
                    .parameters
                    .iter()
                    .map(|ty| external_type(ty, &self.subs))
                    .collect(),
            ),
        }
    }
}

/// A member fact's type as a [`Type`], with the receiver's type arguments
/// in place of the variables they bind.
///
/// A variable nothing bound — a raw receiver, a wrong argument count, a type
/// whose parameters the world does not know — is unknown, and never the
/// `Object` it erases to: that would exclude `log(String)` at
/// `log(raw.get(0))`, which Java's unchecked call picks. A bounded wildcard
/// is unknown too; reading its bound is a different question from what the
/// member returns.
pub(crate) fn external_type(ty: &ExternalType, subs: &HashMap<String, Type>) -> Type {
    external_type_bound(ty, subs, &HashMap::new())
}

/// [`external_type`], with a generic method's own type variables — each an
/// `Unresolved` of its name — as `own` binds them at one call.
fn external_type_bound(
    ty: &ExternalType,
    subs: &HashMap<String, Type>,
    own: &HashMap<String, Type>,
) -> Type {
    match ty {
        ExternalType::Void => Type::Void,
        ExternalType::Primitive(primitive) => Type::Primitive(*primitive),
        ExternalType::Class(fqn) => Type::External {
            name: last_segment(fqn).to_owned(),
            fqn: fqn.clone(),
            arguments: Vec::new(),
        },
        ExternalType::Parameterized(fqn, arguments) => Type::External {
            name: last_segment(fqn).to_owned(),
            fqn: fqn.clone(),
            arguments: arguments
                .iter()
                .map(|argument| external_type_bound(argument, subs, own))
                .collect(),
        },
        ExternalType::Variable(name) => subs
            .get(name)
            .cloned()
            .unwrap_or_else(|| Type::Unknown(name.clone())),
        ExternalType::Extends(_) | ExternalType::Super(_) => Type::Unknown(rendered(ty)),
        // Honest rather than wrong, and the same answer a written name nothing
        // resolves gets from `resolve_written`: the text is carried for a
        // caller that renders it, and nothing is claimed about it. An empty
        // one — a member that writes no type at all — is a value and not an
        // absence, and `Unknown("")` says as little as there is to say.
        ExternalType::Unresolved(written) => own
            .get(written.trim())
            .cloned()
            .unwrap_or_else(|| Type::Unknown(written.clone())),
    }
}

/// A functional-interface parameter as a lambda's target: `? super X` among
/// its arguments is `X`, JLS 9.9's non-wildcard parameterization, where `?
/// extends X` and `?` stay unknown.
fn lambda_target(ty: &ExternalType, subs: &HashMap<String, Type>) -> Type {
    match ty {
        ExternalType::Parameterized(fqn, arguments) => Type::External {
            name: last_segment(fqn).to_owned(),
            fqn: fqn.clone(),
            arguments: arguments
                .iter()
                .map(|argument| match argument {
                    ExternalType::Super(bound) => external_type(bound, subs),
                    other => external_type(other, subs),
                })
                .collect(),
        },
        other => external_type(other, subs),
    }
}

/// The type of a lambda's `position`th parameter, when the lambda is the
/// `argument`th argument of a call `member` was picked for and takes `arity`
/// parameters: the parameter of the single abstract method of the functional
/// interface `member` declares there.
///
/// `None` wherever that is not one certain type — a variadic tail, a
/// parameter that is not an interface type, no single abstract method, an
/// arity that does not match, or a type variable nothing bound.
///
/// ponytail: a parameter typed by the method's *own* variable is unknown
/// even where another argument binds it — `reduce(identity, (a, b) -> …)` —
/// because `infer::bind` runs after the pick this reads. The upgrade is
/// binding the non-lambda arguments first and reading this through them.
/// The functional interface a lambda passed as `argument` is read through.
pub(crate) fn lambda_target_at(
    member: &Member<'_>,
    argument: usize,
    files: &Files<'_>,
    externals: &dyn Externals,
) -> Option<Type> {
    let count = member.parameter_count();
    if member.kind() != DeclKind::Method
        || argument >= count
        || (member.is_variadic() && argument + 1 == count)
    {
        return None;
    }
    match &member.site {
        MemberSite::External(facts) => {
            Some(lambda_target(facts.parameters.get(argument)?, &member.subs))
        }
        MemberSite::Local { .. } => member
            .parameter_types(files, externals)?
            .into_iter()
            .nth(argument),
    }
}

pub(crate) fn lambda_parameter(
    member: &Member<'_>,
    argument: usize,
    position: usize,
    arity: usize,
    strict: bool,
    files: &Files<'_>,
    externals: &dyn Externals,
) -> Option<Type> {
    let target = lambda_target_at(member, argument, files, externals)?;
    if !matches!(target, Type::External { .. } | Type::Declared { .. }) {
        return None;
    }
    // A hint claims the single abstract method only of a list shown whole.
    if strict && !walk_members(&target, files, externals, false).whole_but_object {
        return None;
    }
    // Every interface may redeclare `Object`'s public methods abstractly
    // without them counting, JLS 9.8 — `Comparator.equals`.
    let objects = |candidate: &Member<'_>| {
        matches!(
            (candidate.name(), candidate.parameter_count()),
            ("equals", 1) | ("hashCode", 0) | ("toString", 0)
        )
    };
    let candidates = members_of(&target, files, externals, false);
    // ponytail: a workspace member's abstractness is not indexed, so an
    // interface another file declares — or one inheriting from one — could
    // hide its own abstract method and let an inherited one look single.
    // Nothing is typed through one; the upgrade is an `is_abstract` column on
    // `source_members`. A site with a range is a workspace member, by
    // `ExternalSite::name_range`'s contract.
    if candidates.iter().any(|candidate| {
        candidate
            .external()
            .and_then(|facts| facts.site.as_ref())
            .is_some_and(|site| site.name_range.is_some())
    }) {
        return None;
    }
    let mut abstracts: Vec<(Member<'_>, Vec<Type>)> = Vec::new();
    for candidate in candidates {
        if candidate.kind() != DeclKind::Method || !candidate.is_abstract() || objects(&candidate) {
            continue;
        }
        let parameters = candidate.parameter_types(files, externals)?;
        // A sub-interface redeclaring the one it inherits is still one.
        if !abstracts
            .iter()
            .any(|(seen, types)| seen.name() == candidate.name() && *types == parameters)
        {
            abstracts.push((candidate, parameters));
        }
    }
    let [(method, parameters)] = abstracts.as_slice() else {
        return None;
    };
    if method.parameter_count() != arity || method.is_variadic() {
        return None;
    }
    let ty = parameters.get(position)?.clone();
    (ty.is_known() && !matches!(ty, Type::Variable(_))).then_some(ty)
}

/// Whether `owner` or one of its fields is annotated — what a generator such
/// as Lombok needs to write members into it.
fn carries_an_annotation(owner: &Decl) -> bool {
    let annotated = |decl: &Decl| {
        decl.modifiers
            .iter()
            .any(|modifier| modifier.starts_with('@'))
    };
    annotated(owner)
        || owner
            .children
            .iter()
            .any(|child| child.kind == DeclKind::Field && annotated(child))
}

/// Whether the declaration `owner`, read out of `file`, is the one `site`
/// addresses.
///
/// One function rather than the comparison written twice, because two things
/// turn on it and they answer the same question about the same pair: see
/// [`Member::declared_by`].
fn owned_by(file: &FileId, owner: &Decl, site: &DeclSite) -> bool {
    *file == site.file && owner.name_span == site.span
}

#[cfg(test)]
impl<'a> Member<'a> {
    /// The same member said of another file, for the tests that ask what a
    /// declaration the set cannot reach answers.
    pub(crate) fn resited(&self, file: FileId) -> Self {
        let mut moved = self.clone();
        match &mut moved.site {
            MemberSite::Local { file: at, .. } => *at = file,
            // Loud rather than silent: an external member has no file, so
            // there is nothing to move and a test that asked would be asking
            // the wrong question of it.
            MemberSite::External(_) => panic!("an external member has no file to re-site"),
        }
        moved
    }

    /// The written type of the member's first formal parameter, which is how
    /// `apply.rs`'s tests name the overload that won.
    ///
    /// `None` for an external member, which has no written text: the tests
    /// that name an external winner name it by `ExternalMember::signature`.
    pub(crate) fn first_parameter_type(&self) -> Option<&'a str> {
        match &self.site {
            MemberSite::Local { decl, .. } => decl.parameters.first()?.declared_type.as_deref(),
            MemberSite::External(_) => None,
        }
    }
}

/// The members a receiver of this type can select.
///
/// `statics` picks which half: a receiver naming a type sees static members and
/// nested types, a receiver holding a value sees instance members.
///
/// Two sources answer this. A [`Type::Declared`] is walked here — its
/// declaration, then the supertypes the reachable files declare, with each
/// frame's names resolved in the file that wrote them and its type arguments
/// substituted through. A [`Type::External`] is one question to
/// [`Externals::members`] instead, because no file holds its declaration to
/// walk: the world reports the closure it has already walked, and the answer
/// carries no substitution. Every other type answers with nothing, which is
/// not a failure — an unknown receiver is the ordinary case.
///
/// The two are not exclusive, and that is the commonest shape in a real
/// project rather than a corner: a walked frame whose written supertype
/// resolves to a [`Type::External`] contributes that type's closure, so a
/// domain class extending a framework one answers with both halves in one
/// list. What a caller reads off a [`Member`] says which half it came from,
/// and nothing downstream has to know that the list was merged — including
/// `infer::sole_candidate`, whose own-shadows-inherited rule compares across
/// the seam through `Member::declared_by` alone.
///
/// ponytail: this discards `walk_members`'s wholeness, so every path that
/// *picks* a member is unaware the list it picked from may be short. That is a
/// soundness ceiling and not merely a missing feature: an overload declared
/// only in a supertype the world could not reach is absent from the list, so
/// `infer::sole_candidate` sees one survivor where the whole list would have
/// left two, and picks where it owed a refusal. Completion is unaffected — it
/// lists rather than picks, and reads the flag through
/// [`crate::MemberCompletions`].
///
/// The external-members slice widened it rather than introducing it. Before,
/// only a file-local receiver could pick from a short list; now every
/// [`Type::External`] receiver and every file-local class with an external
/// supertype does too, and a world's gap is a whole type's members rather than
/// one file's. The shape, stated without JDK names because a version can move
/// which type declares what: an external type declares `f(Object)`, its
/// supertype declares `f(String)`, and the world holds a fact for the type but
/// none for the supertype — which is the `None => complete = false` arm of
/// `ClasspathExternals::members`, since the type's own fact still names the
/// supertype it could not reach. `x.f(s)` for a `String` `s` then sees one
/// candidate where the whole list would have left two: `f(Object)`, which a
/// `String` argument does not exclude. `sole_candidate` picks it, and Java's
/// answer is the `f(String)` the walk never reached.
///
/// The workspace-members slice widened it once more, in reach and not in kind:
/// a [`Type::External`] can name a type declared in another source file, and
/// its closure has the same gap wherever one of its written supertypes
/// resolves to nothing. A workspace class extending a type nothing in the
/// workspace or the JDK declares is exactly the shape above.
///
/// The upgrade is *not* to thread this flag into `sole_candidate`, and that is
/// worth saying because the plumbing now looks cheap enough to invite it. The
/// flag is `false` for very nearly every file-local receiver — nobody writes
/// `extends java.lang.Object`, and this walk follows written supertypes only —
/// so gating a pick on it would refuse hover and definition on essentially all
/// local member navigation, which trades a rare wrong pick for a total loss.
///
/// Narrowing the gate to the half where the flag is usually `true` — gate only
/// a [`Type::External`] receiver — does not rescue it either, and that is the
/// version worth ruling out explicitly, because it reads like the obvious
/// compromise. `external_members` answers
/// `complete: found.complete && !dropped && !statics`, so the *static* half of
/// an external receiver is `false` unconditionally: a nested type is a fact of
/// its own and never in the list, which is the `ponytail:` sitting on that
/// same field. A gate confined to external receivers would therefore refuse
/// `Integer.parseInt` and `String.valueOf` outright, which is the same trade
/// one tier down.
///
/// The one caller that does read it is [`crate::missing_member_at`], and it
/// can afford to because its refusal is a missing squiggle rather than a lost
/// navigation: a member diagnostic that a short list silences costs nothing
/// anyone relies on.
///
/// What a correct fix needs is a finer question than "was the list whole":
/// "could a candidate for *this name* be missing", which the closure gaps
/// would have to be attributable to. Contributing the implicit
/// `java.lang.Object` first is the other order, and it makes this flag mean
/// something narrow enough to gate on.
pub fn members_of<'a>(
    ty: &Type,
    files: &Files<'a>,
    externals: &dyn Externals,
    statics: bool,
) -> Vec<Member<'a>> {
    walk_members(ty, files, externals, statics).members
}

/// A member list, and whether the walk that produced it reached everything the
/// type exposes.
///
/// The second field is the whole reason this type exists: the walk below
/// already meets every way of losing a member — a supertype nothing resolved,
/// a file the set cannot reach, an exhausted budget, an external closure the
/// world reports short — and until now it stepped past each of them silently.
/// A caller that wants to *claim* a list is whole has to be told.
pub(crate) struct MemberWalk<'a> {
    pub members: Vec<Member<'a>>,
    /// `false` means members may be missing. It never means what is here is
    /// wrong: every member in the list is one the type really exposes.
    pub complete: bool,
    /// `complete` without the one gap no *method* can fall into: an external
    /// type's static half withholds its nested types, which are facts of
    /// their own. A caller asking whether a method of some name exists reads
    /// this; one listing what a `.` can select reads `complete`.
    pub closed: bool,
    /// `closed`, except for the one gap every file-local type has: the
    /// `java.lang.Object` no declaration writes. A caller that leaves
    /// `Object`'s own names alone may read this instead.
    pub whole_but_object: bool,
}

impl<'a> MemberWalk<'a> {
    /// Nothing found, and nothing shown about why — the answer for a receiver
    /// this walk cannot start on at all.
    fn refused() -> Self {
        Self {
            members: Vec::new(),
            complete: false,
            closed: false,
            whole_but_object: false,
        }
    }
}

/// [`members_of`], reporting whether the list is the whole of what the type
/// exposes.
///
/// Only two kinds of receiver can report a whole list: a [`Type::Declared`]
/// whose walk met none of the losses above and whose every frame wrote down
/// the supertype it has — which in practice means a chain that ends in the
/// world, since `extends java.lang.Object` is never written — and a
/// [`Type::External`] whose world says the closure it reported was closed.
/// Every other receiver — an unknown one, a type variable, an array, a
/// primitive — answers with an empty list and `false`. A primitive really does
/// expose nothing and an array's `length` is simply not modelled, so the two
/// are told apart nowhere: the
/// difference is invisible to a caller that only ever asks "may I claim this
/// list is whole", and refusing both is the answer that cannot be wrong.
pub(crate) fn walk_members<'a>(
    ty: &Type,
    files: &Files<'a>,
    externals: &dyn Externals,
    statics: bool,
) -> MemberWalk<'a> {
    let (decl, arguments) = match ty {
        Type::Declared {
            decl, arguments, ..
        } => (decl, arguments),
        // The walk below has nothing to walk for an external receiver — no
        // file holds its declaration — so the closure is the world's to
        // report and this asks it for one.
        Type::External { fqn, arguments, .. } => {
            return external_members(fqn, arguments, externals, statics);
        }
        _ => return MemberWalk::refused(),
    };
    let Some(root) = files
        .syntax(&decl.file)
        .and_then(|syntax| syntax.declaration_at(decl.span))
    else {
        return MemberWalk::refused();
    };

    let mut out = Vec::new();
    // Lowered wherever the walk steps past something, and never raised again.
    let mut closed = true;
    // `closed` without the implicit `Object`: see `MemberWalk::whole_but_object`.
    let mut whole_but_object = true;
    // An external frame's static half withholds its nested types; see
    // `MemberWalk::closed`.
    let mut withheld = false;
    // Two sets because the two halves are identified two different ways, and
    // one budget over both because a limit spent twice is not the limit it
    // says it is — see `MAX_SUPERTYPES`. A declaration is keyed on its site
    // and not its span: the same span in two files is two declarations, and
    // keying on the span alone would end a cross-file walk early and drop the
    // members it had not reached yet. A fact has no site, so an external
    // supertype is keyed on the one thing that identifies it — its
    // fully-qualified name — which is what makes a diamond onto the same
    // framework type contribute once. Neither `Vec` allocates until something
    // is pushed, so a hierarchy with no external supertype pays for nothing.
    let mut seen: Vec<DeclSite> = Vec::new();
    let mut asked: Vec<String> = Vec::new();
    // (the frame's file, its declaration, the substitution that applies
    // inside, whether it is raw). JLS 4.8: a raw type's supertypes are
    // erased, so every edge out of a raw frame binds nothing, and a generic
    // supertype reached that way is raw itself.
    let raw = arguments.is_empty() && !type_parameter_names(root).is_empty();
    let mut queue = vec![(
        decl.file.clone(),
        root,
        Rc::new(substitution(root, arguments)),
        raw,
    )];

    while let Some((file, owner, subs, raw)) = queue.pop() {
        let site = DeclSite::new(file.clone(), owner.name_span);
        // The two terms of the old single guard, split because they mean
        // opposite things to a caller asking whether the list is whole: a
        // frame already visited costs nothing, and a frame the budget refuses
        // is members going missing. Both still `continue`, so the list itself
        // is unchanged.
        if seen.contains(&site) {
            continue;
        }
        if seen.len() + asked.len() >= MAX_SUPERTYPES {
            closed = false;
            whole_but_object = false;
            continue;
        }
        // Whether this frame is a supertype rather than the type asked about,
        // which is what decides whether an interface's static method is
        // reachable at all. Through `owned_by`, so this reads the same as
        // `Member::declared_by` — the rule that shadows an inherited member
        // must not disagree with the walk about which frame was inherited.
        let inherited = !owned_by(&file, owner, decl);
        seen.push(site);

        for child in &owner.children {
            if !is_member(child.kind) || child.name.is_empty() {
                continue;
            }
            if is_static(child, owner) != statics {
                continue;
            }
            // Java does not inherit an interface's static methods, so offering
            // one on an implementer offers code that will not compile.
            // Constants are inherited, which is why only methods are skipped.
            if inherited
                && owner.kind == DeclKind::Interface
                && child.kind == DeclKind::Method
                && child.modifiers.iter().any(|modifier| modifier == "static")
            {
                continue;
            }
            out.push(Member {
                site: MemberSite::Local {
                    decl: child,
                    owner,
                    file: file.clone(),
                },
                subs: Rc::clone(&subs),
                closure: None,
                generated_into: None,
            });
        }
        // What a generator writes into this declaration — Lombok's getters —
        // which no file holds as a declaration, so the world answers it as
        // facts. Only a type carrying an annotation can have any, which keeps
        // the fqn off the walk's path everywhere else.
        // An enum's `values()` and `valueOf` and a record's accessors arrive
        // the same way: JLS 8.9.3 and 8.10.3 declare them and no file writes
        // them.
        let implicit = matches!(owner.kind, DeclKind::Enum | DeclKind::Record);
        let mut generated = Vec::new();
        if (implicit || carries_an_annotation(owner))
            && let Some(fqn) = files.syntax(&file).and_then(|syntax| {
                syntax
                    .named_types()
                    .into_iter()
                    .find(|named| std::ptr::eq(named.decl, owner))
                    .map(|named| named.fqn)
            })
        {
            generated = externals.generated_members(&fqn, &file);
        }
        // They are the index's, a quarter-second behind the buffer or absent
        // for a buffer it does not hold: the list is whole only where they
        // cover what the declaration writes now.
        let covered = match owner.kind {
            DeclKind::Enum => {
                !statics
                    || generated
                        .iter()
                        .any(|facts| facts.is_static && facts.name == "values")
            }
            DeclKind::Record => {
                statics
                    || owner
                        .children
                        .iter()
                        .filter(|child| child.kind == DeclKind::RecordComponent)
                        .all(|component| {
                            let accessor = |kind: DeclKind, name: &str, arity: usize| {
                                kind == DeclKind::Method && name == component.name && arity == 0
                            };
                            owner.children.iter().any(|child| {
                                accessor(child.kind, &child.name, child.parameters.len())
                            }) || generated.iter().any(|facts| {
                                !facts.is_static
                                    && accessor(facts.kind, &facts.name, facts.parameters.len())
                            })
                        })
            }
            _ => true,
        };
        if !covered {
            closed = false;
            whole_but_object = false;
        }
        for facts in generated {
            if facts.is_static != statics {
                continue;
            }
            out.push(Member {
                site: MemberSite::External(Arc::new(facts)),
                subs: Rc::clone(&subs),
                closure: None,
                generated_into: Some((owner, file.clone())),
            });
        }

        // A declaration that writes no superclass has one regardless, and this
        // walk follows written names only: JLS 8.1.4 puts every class under
        // `java.lang.Object`, 8.9 and 8.10 put an enum and a record under
        // `Enum` and `Record`, and JLS 9.2 gives an interface `Object`'s public
        // methods without an `extends` to reach them by — the parser puts an
        // interface's own `extends` list in `implements`, so this is `None`
        // for every interface. So `toString` and its siblings are missing from
        // this list whatever else the walk manages, and a caller cannot claim
        // it is whole.
        //
        // ponytail: a refusal, not the fix. The fix is to resolve the implicit
        // supertype through `Externals` and contribute its closure, which the
        // `Type::External` arm below already knows how to do — but it *adds
        // members* to every file-local `.` in the editor, which is a change to
        // an answer rather than to a flag, so it belongs to a slice that can
        // walk the whole feature rather than to this one.
        //
        // An enum's and a record's are the exception: `java.lang.Enum` and
        // `java.lang.Record` are the world's, so the closure is contributed
        // as a written external supertype's is — raw, since `Enum<E>`'s
        // argument is the enum itself — and the walk can be whole.
        let implicit = match owner.kind {
            DeclKind::Enum => Some("java.lang.Enum"),
            DeclKind::Record => Some("java.lang.Record"),
            _ => None,
        };
        match implicit {
            Some(fqn) if !asked.iter().any(|name| name == fqn) => {
                asked.push(fqn.to_owned());
                let inherited = inherited_external_members(fqn, &[], externals, statics, false);
                closed &= inherited.closed;
                whole_but_object &= inherited.closed;
                withheld |= !inherited.complete;
                out.extend(inherited.members);
            }
            Some(_) => {}
            // An anonymous class's supertype is written as its name, which
            // this walk does not follow: what it inherits is missing.
            None if owner.kind == DeclKind::AnonymousClass => {
                closed = false;
                whole_but_object = false;
            }
            None if owner.extends.is_none() => closed = false,
            None => {}
        }
        // A supertype written in this frame's file means whatever that file's
        // package, imports and scopes say it means, so it is resolved from
        // there and never from the file the question was asked in. A frame
        // whose file the set cannot reach is skipped, exactly as a supertype
        // nothing resolves is.
        let Some(view) = files.viewing(&file) else {
            // The frame's own members are already in the list; what is lost is
            // whatever it inherits, because nothing here can read the file its
            // supertype names were written in.
            closed = false;
            whole_but_object = false;
            continue;
        };
        // `extends` and `implements` are read separately for one reason: which
        // of the two a name was written in is what says whether it names an
        // interface, and that decides whether its static methods are
        // inherited. The parser has already normalised the awkward case — an
        // interface's own `extends` list lands in `implements` — so `extends`
        // is a superclass and nothing else.
        let written_supertypes = owner
            .extends
            .iter()
            .map(|written| (written, false))
            .chain(owner.implements.iter().map(|written| (written, true)));
        for (written, is_interface) in written_supertypes {
            let at = owner.name_span.start_byte;
            let mut node_budget = MAX_TYPE_NODES;
            let resolved = substitute(
                &resolve_written(written, &view, externals, at),
                &subs,
                &mut node_budget,
            );
            match &resolved {
                Type::Declared {
                    decl, arguments, ..
                } => {
                    let Some(supertype) = files
                        .syntax(&decl.file)
                        .and_then(|syntax| syntax.declaration_at(decl.span))
                    else {
                        closed = false;
                        whole_but_object = false;
                        continue;
                    };
                    // A *superclass* already visited is a cycle, and the one
                    // way a chain can end without ending at `java.lang.Object`
                    // — so it has to be caught here rather than by the
                    // unwritten-supertype refusal above, which a cycle never
                    // reaches: every frame in one writes an `extends` that
                    // resolves. Java is single-inheritance, so the same class
                    // twice in one chain cannot be anything but a cycle, and
                    // reaching an *interface* twice is the ordinary diamond the
                    // `seen` guard exists to absorb and costs nothing. Invalid
                    // Java either way, and ordinary input to a parser that
                    // never rejects.
                    if !is_interface
                        && seen.contains(&DeclSite::new(decl.file.clone(), supertype.name_span))
                    {
                        closed = false;
                        whole_but_object = false;
                    }
                    let (subs, raw) = if raw {
                        (HashMap::new(), !type_parameter_names(supertype).is_empty())
                    } else {
                        (substitution(supertype, arguments), false)
                    };
                    queue.push((decl.file.clone(), supertype, Rc::new(subs), raw));
                }
                // A closure rather than a frame, so it is contributed here and
                // never queued: the world has already walked what is above it,
                // and there is no declaration for the loop to read children
                // off. Charged and keyed like a frame all the same.
                //
                // The guard keeps the first arrival's `is_interface` and
                // discards the second's, so a type reached once through
                // `extends` and once through `implements` answers by whichever
                // the walk met first. Only reachable from input that does not
                // compile — a type is not both a class and an interface — and
                // the same fact-kind upgrade `inherited_external_members`
                // names would settle it.
                Type::External { fqn, arguments, .. } => {
                    // Split for the same reason the frame guard above is: a
                    // second arrival at one fqn is the diamond this set exists
                    // to absorb, and the budget refusing one is a closure
                    // going missing.
                    if asked.iter().any(|name| name == fqn) {
                        continue;
                    }
                    if seen.len() + asked.len() >= MAX_SUPERTYPES {
                        closed = false;
                        whole_but_object = false;
                        continue;
                    }
                    asked.push(fqn.clone());
                    let inherited = inherited_external_members(
                        fqn,
                        if raw { &[] } else { arguments },
                        externals,
                        statics,
                        is_interface,
                    );
                    closed &= inherited.closed;
                    whole_but_object &= inherited.closed;
                    withheld |= !inherited.complete;
                    out.extend(inherited.members);
                }
                // A supertype nothing resolved, a type variable nothing bound,
                // an array: nothing to walk and nothing to ask, exactly as
                // before — and a member list that is short by everything that
                // supertype declares.
                //
                // `MAX_TYPE_NODES` reaches this only when the *top* of the
                // substituted type collapses, which is what makes the whole
                // supertype unresolvable. Running out deeper down leaves a
                // `Declared` whose argument is unknown: the frame still walks
                // and contributes every member, and what degrades is a
                // member's rendered type rather than the list.
                _ => {
                    closed = false;
                    whole_but_object = false;
                }
            }
        }
    }
    MemberWalk {
        members: out,
        complete: closed && !withheld,
        closed,
        whole_but_object,
    }
}

/// The members the world reports for one external type, in the half `statics`
/// asks for — everything a receiver of it can select.
///
/// Which is the whole answer for an external *receiver* and not quite the whole
/// of it for an external *supertype* of a file-local class: see
/// [`inherited_external_members`], which is the one rule that turns on the
/// difference.
///
/// One question and no walk. What the trait hands back is already closed over
/// the supertype chain, and the two rules that closure has to obey — an
/// interface's static method is not inherited, and a member's `owner` names
/// the type that declares it — are stated on [`Externals::members`] and
/// applied where the facts are. Nothing here has a kind or a modifier to
/// apply them with, and re-deriving them from a member list that has already
/// been flattened is not possible: which frame a member came from is exactly
/// what flattening dropped.
///
/// `arguments` are the receiver's type arguments, which bind the type's own
/// parameters: the world expresses every member it reports — inherited ones
/// included — in those, so `List<String>.get` answers `String` and so does the
/// `get` a file-local `class Names extends ArrayList<String>` inherits. A
/// receiver with no arguments, or the wrong number, binds nothing.
fn external_members<'a>(
    fqn: &str,
    arguments: &[Type],
    externals: &dyn Externals,
    statics: bool,
) -> MemberWalk<'a> {
    let Some(mut found) = externals.members(fqn) else {
        // "Cannot say", not "nothing": a world that holds no fact for this
        // name is the empty-world case, and every member of the type is
        // missing rather than absent.
        return MemberWalk::refused();
    };
    // A raw receiver, JLS 4.8: its supertypes are erased, so no edge binds,
    // and what is left is erased too — `subList` is a raw `List`, and a
    // variable is unknown, never the `Object` javac would say.
    if arguments.is_empty() && !found.type_parameters.is_empty() {
        let Some(raw) = externals.raw_members(fqn) else {
            return MemberWalk::refused();
        };
        found = raw;
        // A static member names no type variable of its class (JLS 8.1.3),
        // so a raw access erases nothing of it — and its own type variables
        // are a call's to bind, `Optional.of(s)`'s `T` among them.
        for member in found.members.iter_mut().filter(|member| !member.is_static) {
            let member = Arc::make_mut(member);
            // An instance method's own type variables are erased with the
            // rest of it: `raw.pick(s)` is not a `String`.
            member.type_parameters.clear();
            member.declared = erased(&member.declared);
            for parameter in &mut member.parameters {
                *parameter = erased(parameter);
            }
        }
    }
    // One substitution for the whole list rather than one per member: a
    // god-class's member list is thousands long.
    let subs: HashMap<String, Type> = if found.type_parameters.len() == arguments.len() {
        found
            .type_parameters
            .iter()
            .cloned()
            .zip(arguments.iter().cloned())
            .collect()
    } else {
        HashMap::new()
    };
    let subs = Rc::new(subs);
    let closure: Rc<str> = Rc::from(fqn);
    let mut dropped = false;
    let members = found
        .members
        .into_iter()
        // A backstop rather than a rule: the trait owes the same list, and
        // stating it there is what makes a dropped member findable. Dropping
        // one here is still better than the alternative, which is a
        // constructor offered in a completion list and rendered without its
        // parentheses. Dropping one is also a member missing, which is why the
        // backstop firing is worth one `bool` to notice.
        .filter(|member| {
            let admitted = is_external_member_kind(member.kind);
            dropped |= !admitted;
            admitted
        })
        // Not a loss: the half the caller did not ask for is the other
        // namespace, not a gap in this one.
        .filter(|member| member.is_static == statics)
        .map(|member| Member {
            site: MemberSite::External(member),
            subs: Rc::clone(&subs),
            closure: Some(Rc::clone(&closure)),
            generated_into: None,
        })
        .collect();
    MemberWalk {
        members,
        // The static half can never be shown whole, and the reason is
        // [`Externals::members`]'s own: a nested type is a fact of its own
        // reached by its own name and is deliberately not in the list. That is
        // no loss to a *value* receiver, which cannot select a type through a
        // `.` at all, but `Map.` can select `Entry` and this list does not
        // carry it.
        //
        // ponytail: withheld rather than filled in. Offering an external
        // nested type means the world enumerating a type's nested facts —
        // `TypeWorld` is keyed by fqn and can find `java.util.Map.Entry` from
        // `java.util.Map` by prefix, but doing it per `.` on the request path
        // wants an index built once. It adds members rather than a flag, so it
        // belongs to the slice that can walk the feature.
        complete: found.complete && !dropped && !statics,
        closed: found.complete && !dropped,
        whole_but_object: found.complete && !dropped,
    }
}

/// The constructors `new ty(…)` chooses among — `ty`'s own, never an
/// inherited one (JLS 8.8) — bound to `ty`'s type arguments, or `None` where
/// the list may be short: a lone candidate is picked, so a list that is not
/// whole would make a guess of a refusal.
///
/// Whole for a declaration that writes its constructors and that no
/// generator may add one to — [`Decl::writes_its_constructors`] — and for an
/// external type wherever [`Externals::constructors`] answers.
pub(crate) fn constructors_of<'a>(
    ty: &Type,
    files: &Files<'a>,
    externals: &dyn Externals,
) -> Option<Vec<Member<'a>>> {
    match ty {
        Type::Declared {
            decl, arguments, ..
        } => {
            let owner = files.syntax(&decl.file)?.declaration_at(decl.span)?;
            if !owner.writes_its_constructors() {
                return None;
            }
            let subs = Rc::new(substitution(owner, arguments));
            Some(
                owner
                    .children
                    .iter()
                    .filter(|child| child.kind == DeclKind::Constructor)
                    .map(|child| Member {
                        site: MemberSite::Local {
                            decl: child,
                            owner,
                            file: decl.file.clone(),
                        },
                        subs: Rc::clone(&subs),
                        closure: None,
                        generated_into: None,
                    })
                    .collect(),
            )
        }
        Type::External { fqn, arguments, .. } => {
            let found = externals.constructors(fqn)?;
            // A raw or diamond `new` binds nothing, and a parameter naming a
            // variable stays one, which excludes nothing.
            let subs: HashMap<String, Type> = if found.type_parameters.len() == arguments.len() {
                found
                    .type_parameters
                    .iter()
                    .cloned()
                    .zip(arguments.iter().cloned())
                    .collect()
            } else {
                HashMap::new()
            };
            let subs = Rc::new(subs);
            let closure: Rc<str> = Rc::from(fqn.as_str());
            Some(
                found
                    .members
                    .into_iter()
                    .filter(|facts| facts.kind == DeclKind::Constructor)
                    .map(|facts| Member {
                        site: MemberSite::External(facts),
                        subs: Rc::clone(&subs),
                        closure: Some(Rc::clone(&closure)),
                        generated_into: None,
                    })
                    .collect(),
            )
        }
        _ => None,
    }
}

/// A member type of a raw receiver: a parameterized type is its raw type,
/// and a variable or a wildcard claims nothing.
fn erased(ty: &ExternalType) -> ExternalType {
    match ty {
        ExternalType::Parameterized(fqn, _) => ExternalType::Class(fqn.clone()),
        ExternalType::Variable(_) | ExternalType::Extends(_) | ExternalType::Super(_) => {
            ExternalType::Unresolved(rendered(ty))
        }
        other => other.clone(),
    }
}

/// The members a file-local frame inherits from an external supertype, in the
/// half `statics` asks for.
///
/// One rule of its own on top of [`external_members`], and the far side of the
/// trait cannot supply it. `Externals::members` applies the interface-static
/// rule *within* the closure it walks — a supertype frame that is an interface
/// contributes no static method — but the type asked about is not a supertype
/// of itself, so its own static methods are in the answer, correctly, since a
/// receiver naming that interface can select them. Reached as a file-local
/// class's supertype they must not be: JLS 8.4.8 does not inherit an
/// interface's static method, and offering one on an implementer offers code
/// that will not compile. Constants are inherited, which is why only methods
/// are dropped — the same rule and the same exception as the file-local walk
/// applies to a `DeclKind::Interface` frame.
///
/// `is_interface` comes from which supertype list the name was written in
/// rather than from a fact, because no fact says: the trait exposes a type's
/// members and not its kind. In a file that compiles the position answers, so
/// a class supertype keeps its inherited static methods rather than losing
/// them to a conservative guess.
///
/// ponytail: the position answers for the *keyword* and not the kind, so an
/// `extends` naming an interface — `class Config extends Map {}`, invalid Java
/// and ordinary input to a parser that never rejects — over-offers that
/// interface's static methods on the implementer. The upgrade is a kind on the
/// fact, which `ExternalMembers` does not carry today.
fn inherited_external_members<'a>(
    fqn: &str,
    arguments: &[Type],
    externals: &dyn Externals,
    statics: bool,
    is_interface: bool,
) -> MemberWalk<'a> {
    let mut found = external_members(fqn, arguments, externals, statics);
    if is_interface && statics {
        // Not a loss either: an interface's static method is not inherited, so
        // a list without it is the complete list of what is reachable here.
        found
            .members
            .retain(|member| member.kind() != DeclKind::Method);
    }
    found
}

/// Which member facts a `.` can reach: [`is_member`]'s list minus the type
/// kinds, which is what [`Externals::members`] states as an obligation.
///
/// A nested type is where the two lists differ, and deliberately: a file's
/// nested type is a child of the declaration the walk is on, so the walk is
/// the only thing that can offer it, where an external one is a type fact of
/// its own reached by its own name — offering it here would offer it twice.
fn is_external_member_kind(kind: DeclKind) -> bool {
    is_member(kind) && !is_type_kind(kind)
}

/// Which declarations are members a `.` can reach. Locals and parameters live
/// in `children` too, because a method body's declarations nest there.
fn is_member(kind: DeclKind) -> bool {
    is_type_kind(kind)
        || matches!(
            kind,
            DeclKind::Field
                | DeclKind::Method
                | DeclKind::EnumConstant
                | DeclKind::RecordComponent
                | DeclKind::AnnotationElement
        )
}

/// Whether a declaration is itself a type, rather than a member that has one.
/// Shared by `is_member` (a nested type is reachable through the outer
/// type's name) and `is_static` (for the same reason, it is always static).
fn is_type_kind(kind: DeclKind) -> bool {
    matches!(
        kind,
        DeclKind::Class
            | DeclKind::Interface
            | DeclKind::Enum
            | DeclKind::Record
            | DeclKind::AnnotationType
            | DeclKind::AnonymousClass
    )
}

/// Whether a member answers to a receiver naming its type rather than holding
/// a value.
///
/// Three things make a member static and only one is the keyword: a nested
/// type and an enum constant are reached through the type's name, and a field
/// of an interface or annotation type is implicitly static whether or not the
/// author wrote it.
fn is_static(decl: &Decl, owner: &Decl) -> bool {
    if is_type_kind(decl.kind) || decl.kind == DeclKind::EnumConstant {
        return true;
    }
    if matches!(owner.kind, DeclKind::Interface | DeclKind::AnnotationType)
        && matches!(decl.kind, DeclKind::Field | DeclKind::AnnotationElement)
    {
        return true;
    }
    decl.modifiers.iter().any(|modifier| modifier == "static")
}

/// The names a declaration's own type parameters bind, e.g. `["T"]` for
/// `<T extends Number>`.
///
/// `strip_prefix`/`strip_suffix` rather than `trim_*_matches`, which strips
/// repeated delimiters and would unbalance a nested bound like
/// `<K, V extends Map<K, V>>`. `split_arguments` rather than `split(',')` for
/// the same reason: the comma inside that bound is not the one separating
/// parameters.
fn type_parameter_names(decl: &Decl) -> Vec<String> {
    let Some(written) = &decl.type_parameters else {
        return Vec::new();
    };
    let inner = written.strip_prefix('<').unwrap_or(written);
    let inner = inner.strip_suffix('>').unwrap_or(inner);
    split_arguments(inner)
        .into_iter()
        // `T extends Number` binds `T`.
        .filter_map(|part| part.split_whitespace().next().map(str::to_owned))
        .collect()
}

/// Bind a declaration's type parameters to the arguments a use supplied.
///
/// `class Box<T>` used as `Box<String>` binds `T` to `String`. A use with no
/// arguments — a raw type — binds nothing, and `T` stays a `Type::Variable`.
fn substitution(decl: &Decl, arguments: &[Type]) -> HashMap<String, Type> {
    type_parameter_names(decl)
        .into_iter()
        .zip(arguments.iter().cloned())
        .collect()
}

/// Replace type variables with what the use bound them to. A variable nothing
/// bound stays a variable rather than becoming unknown: it is still a name the
/// reader recognises.
///
/// `budget` bounds how many type nodes this call may produce; see
/// `MAX_TYPE_NODES`. A bound value is charged against that same budget with
/// `charge` before it is spliced in, rather than pasted in for free: a chain
/// where each level's argument doubles the last (`class A0<T> extends
/// A1<Pair<T, T>>`, `class A1<U> extends A2<Pair<U, U>>`, ...) inserts an
/// exponentially large value in a single clone, and counting only the
/// unsubstituted skeleton's own handful of nodes never sees that coming —
/// the skeleton is the same small size at every level, only the value being
/// spliced into it grows.
fn substitute(ty: &Type, subs: &HashMap<String, Type>, budget: &mut u32) -> Type {
    if *budget == 0 {
        return Type::Unknown(String::new());
    }
    *budget -= 1;
    match ty {
        Type::Variable(name) => match subs.get(name) {
            Some(bound) if charge(bound, budget) => bound.clone(),
            Some(_) => Type::Unknown(String::new()),
            None => ty.clone(),
        },
        Type::Array(element) => Type::Array(Box::new(substitute(element, subs, budget))),
        Type::Declared {
            name,
            decl,
            arguments,
        } => Type::Declared {
            name: name.clone(),
            decl: decl.clone(),
            arguments: arguments
                .iter()
                .map(|argument| substitute(argument, subs, budget))
                .collect(),
        },
        // An external type's arguments bind its members' variables, so the
        // `T` in a field declared `List<T>` on `Holder<Foo>` is `Foo`.
        Type::External {
            name,
            fqn,
            arguments,
        } => Type::External {
            name: name.clone(),
            fqn: fqn.clone(),
            arguments: arguments
                .iter()
                .map(|argument| substitute(argument, subs, budget))
                .collect(),
        },
        other => other.clone(),
    }
}

/// Whether `ty` fits inside what remains of `budget`, decrementing it once per
/// node and stopping — via `Iterator::all`'s short-circuit — the moment it
/// runs out rather than walking the rest of a value that may be enormous.
/// A value too large to afford is therefore never even fully visited, let
/// alone cloned.
fn charge(ty: &Type, budget: &mut u32) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    match ty {
        Type::Array(element) => charge(element, budget),
        Type::Declared { arguments, .. } | Type::External { arguments, .. } => {
            arguments.iter().all(|argument| charge(argument, budget))
        }
        _ => true,
    }
}

/// Substitute a member's declared type, minus any name the member re-declares
/// as its own type parameter: `<T> T get()` on `class Holder<T>` must not have
/// its `T` rewritten by `Holder`'s substitution, because it is a different `T`
/// that merely shares the name.
fn substitute_member(decl: &Decl, ty: &Type, subs: &HashMap<String, Type>) -> Type {
    let mut budget = MAX_TYPE_NODES;
    let shadowed = type_parameter_names(decl);
    if shadowed.is_empty() {
        return substitute(ty, subs, &mut budget);
    }
    let narrowed: HashMap<String, Type> = subs
        .iter()
        .filter(|(name, _)| !shadowed.contains(name))
        .map(|(name, ty)| (name.clone(), ty.clone()))
        .collect();
    substitute(ty, &narrowed, &mut budget)
}

/// A member's type: its declared type, resolved where it was declared, with
/// its owner's substitution applied.
///
/// A nested type and an enum constant are the exception: the parser records
/// no declared-type text for either, because neither has one to write. Their
/// own name — or, for a constant, the enum declaring it — is the answer
/// instead of a resolved one.
///
/// Resolved here rather than by `members_of`, because resolving a written type
/// runs `FileSyntax::declaration_at`, which walks the *whole file's*
/// declaration tree. Doing that for every member of every frame is
/// O(members × file) on the request loop — at 8 000 members it was 31 ms
/// before any parse cost, against a 50 ms completion budget — and the only
/// caller that wants a member's type filters candidates by name and arity
/// first, so all but one or two of those resolutions were thrown away.
pub fn member_type(member: &Member<'_>, files: &Files<'_>, externals: &dyn Externals) -> Type {
    match &member.site {
        MemberSite::Local { decl, owner, file } => {
            resolved_type(decl, owner, file, &member.subs, files, externals)
        }
        // Nothing to resolve: whatever built the fact resolved the type
        // already, or said it could not. The receiver's arguments are
        // substituted in.
        MemberSite::External(facts) => external_type(&facts.declared, &member.subs),
    }
}

/// [`member_type`] at one call, with the member's own type variables as
/// `own` binds them — see `infer::bind`.
pub(crate) fn member_type_bound(
    member: &Member<'_>,
    files: &Files<'_>,
    externals: &dyn Externals,
    own: &HashMap<String, Type>,
) -> Type {
    match &member.site {
        MemberSite::External(facts) => external_type_bound(&facts.declared, &member.subs, own),
        MemberSite::Local { .. } => member_type(member, files, externals),
    }
}

fn resolved_type(
    decl: &Decl,
    owner: &Decl,
    file: &FileId,
    subs: &HashMap<String, Type>,
    files: &Files<'_>,
    externals: &dyn Externals,
) -> Type {
    if is_type_kind(decl.kind) {
        return Type::Declared {
            name: decl.name.clone(),
            decl: DeclSite::new(file.clone(), decl.name_span),
            arguments: Vec::new(),
        };
    }
    if decl.kind == DeclKind::EnumConstant {
        return Type::Declared {
            name: owner.name.clone(),
            decl: DeclSite::new(file.clone(), owner.name_span),
            arguments: Vec::new(),
        };
    }
    let Some(written) = &decl.declared_type else {
        // Honest rather than wrong: `Void` would claim we know the member
        // returns nothing, and nothing reaching here does.
        return Type::Unknown(decl.name.clone());
    };
    // Resolved in the member's own file, for the reason the supertype walk in
    // `members_of` resolves a frame's supertypes in the frame's file: the
    // declared type is written there, so that file's package, imports and
    // scopes are what it means against.
    let Some(view) = files.viewing(file) else {
        return Type::Unknown(decl.name.clone());
    };
    let resolved = resolve_written(written, &view, externals, decl.name_span.start_byte);
    substitute_member(decl, &resolved, subs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoExternals;
    use crate::externals::testing::{World, class, field, method};
    use crate::types::Primitive;
    use javelin_document::{DocumentId, TextDocument};
    use javelin_parser::{FileSyntax, JavaParser, ScopeTree};

    /// The file every single-file fixture below is, and the *current* file of
    /// the cross-file ones.
    const FILE: &str = "file:///A.java";

    /// The second file the cross-file tests reach into. Never the current
    /// file: the point of those tests is that the walk leaves `FILE`.
    const OTHER: &str = "file:///B.java";

    /// A file the set never holds, for the tests that ask what an
    /// unreachable declaration answers.
    const ABSENT: &str = "file:///Absent.java";

    fn set<'a>(syntax: &'a FileSyntax, scopes: &'a ScopeTree) -> Files<'a> {
        Files::single(FileId::new(FILE), syntax, scopes)
    }

    /// A set whose current file is [`FILE`] with `a`'s views, and which can
    /// also reach [`OTHER`] with `b`'s.
    fn two<'a>(a: &'a (FileSyntax, ScopeTree), b: &'a (FileSyntax, ScopeTree)) -> Files<'a> {
        let mut set = Files::single(FileId::new(FILE), &a.0, &a.1);
        set.add(FileId::new(OTHER), &b.0, &b.1);
        set
    }

    /// The `Type::Declared` for `type_name` as the file called `file`
    /// declares it — a site naming *that* file, which is the only way to hand
    /// a walk a receiver in another file: `resolve_written` asks the current
    /// file and then `Externals`, and deliberately never searches the set's
    /// other entries for a matching name.
    ///
    /// Resolved in a one-file set of its own rather than through
    /// `Files::viewing`, so that a test's setup does not depend on the very
    /// function the test is about.
    fn declared_in(
        file: &str,
        views: &(FileSyntax, ScopeTree),
        source: &str,
        type_name: &str,
    ) -> Type {
        let one = Files::single(FileId::new(file), &views.0, &views.1);
        let at = source.len() as u32 - 1;
        crate::resolve_written(type_name, &one, &NoExternals, at)
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

    /// The declaration a member is declared on. Reached through the private
    /// site rather than an accessor, because nothing outside this module has
    /// a use for the owner itself — [`Member::declared_by`] is the question
    /// the crate actually asks about it.
    fn owner_of<'a>(member: &Member<'a>) -> &'a Decl {
        match &member.site {
            MemberSite::Local { owner, .. } => owner,
            // An external member's owner is a name, not a declaration:
            // `Member::external` is what reads it, and
            // `an_inherited_external_member_is_offered_alongside_a_declared_one`
            // is what asks.
            MemberSite::External(_) => panic!("an external member has no declaring `Decl`"),
        }
    }

    /// The names `ty`'s members go by, sorted so a test can compare them.
    ///
    /// The one place a member list is turned into names: the three ways a test
    /// below reaches a receiver — a type it built, an external name, a name
    /// resolved out of a source string — all differ in how they get to the
    /// `Type` and not in what they ask of it.
    fn member_names(
        ty: &Type,
        files: &Files<'_>,
        world: &dyn Externals,
        statics: bool,
    ) -> Vec<String> {
        let mut names: Vec<String> = members_of(ty, files, world, statics)
            .into_iter()
            .map(|member| member.name().to_owned())
            .collect();
        names.sort();
        names
    }

    /// The two views a `Files` entry needs. The `TextDocument` is named
    /// [`FILE`] whichever file the views are going to stand for: nothing
    /// reads a document's id back off a `FileSyntax` or a `ScopeTree`, and
    /// the id that decides anything is the one the set is keyed on.
    fn parts(source: &str) -> (FileSyntax, ScopeTree) {
        let document = TextDocument::new(DocumentId::new(FILE), 1, source.to_owned());
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        (
            FileSyntax::build(&document, &tree),
            ScopeTree::build(&document, &tree),
        )
    }

    const STRING: &str = "java.lang.String";
    const OBJECT: &str = "java.lang.Object";
    const ARRAY_LIST: &str = "java.util.ArrayList";

    /// A receiver of the external type `fqn`, built rather than resolved: what
    /// these tests are about starts once the receiver has a type, and
    /// `resolve_written` needs a file whose imports name it to get there.
    fn external(fqn: &str) -> Type {
        Type::External {
            name: last_segment(fqn).to_owned(),
            fqn: fqn.to_owned(),
            arguments: Vec::new(),
        }
    }

    /// The names an external receiver's members go by, sorted. The `Files` set
    /// is scenery: the external arm of [`members_of`] never reads it, which is
    /// most of why it can answer at all.
    fn names_in(ty: &Type, world: &dyn Externals, statics: bool) -> Vec<String> {
        let (syntax, scopes) = parts("class A {}");
        member_names(ty, &set(&syntax, &scopes), world, statics)
    }

    /// A world where `java.lang.String` has one member of each half.
    fn split_world() -> World {
        World::default().with(
            STRING,
            &[],
            vec![
                method(
                    STRING,
                    "length",
                    ExternalType::Primitive(Primitive::Int),
                    vec![],
                ),
                {
                    let mut order = field(STRING, "CASE_INSENSITIVE_ORDER", class(STRING));
                    order.is_static = true;
                    order
                },
            ],
        )
    }

    /// The type of `java.lang.String`'s member called `name`, in `world`.
    fn typed(world: &dyn Externals, name: &str) -> Type {
        let (syntax, scopes) = parts("class A {}");
        let files = set(&syntax, &scopes);
        let members = members_of(&external(STRING), &files, world, false);
        let member = members
            .iter()
            .find(|member| member.name() == name)
            .unwrap_or_else(|| panic!("no external member called {name}"));
        member_type(member, &files, world)
    }

    /// Resolve a type by name at the very end of the file, where every
    /// top-level declaration is in scope, and list its members — with `world`
    /// answering for whatever the file does not declare, on both the receiver's
    /// own name and its supertypes'.
    fn names_with(
        source: &str,
        type_name: &str,
        world: &dyn Externals,
        statics: bool,
    ) -> Vec<String> {
        let (syntax, scopes) = parts(source);
        let files = set(&syntax, &scopes);
        let at = source.len() as u32 - 1;
        let ty = crate::resolve_written(type_name, &files, world, at);
        member_names(&ty, &files, world, statics)
    }

    /// The same with nothing outside the file, which is what most of the tests
    /// below want.
    fn names_of(source: &str, type_name: &str, statics: bool) -> Vec<String> {
        names_with(source, type_name, &NoExternals, statics)
    }

    /// JLS 8.9.3 and 8.10.3: what Java declares into an enum or a record
    /// arrives with the generated members and `Enum`'s and `Record`'s
    /// closures with the world, and the walk is whole only with both.
    #[test]
    fn an_enum_or_a_record_is_whole_with_what_java_declares_into_it() {
        let source = "enum Day { MON } record Entry(String date) {}";
        let (syntax, scopes) = parts(source);
        let files = set(&syntax, &scopes);
        let world = |generated: bool| {
            let world = World::default()
                .with("java.lang.Object", &[], vec![])
                .with(
                    "java.lang.Enum",
                    &["java.lang.Object"],
                    vec![method(
                        "java.lang.Enum",
                        "name",
                        class("java.lang.String"),
                        vec![],
                    )],
                )
                .with("java.lang.Record", &["java.lang.Object"], vec![])
                .with("java.lang.String", &["java.lang.Object"], vec![]);
            if !generated {
                return world;
            }
            let mut values = method(
                "Day",
                "values",
                ExternalType::Unresolved("Day[]".to_owned()),
                vec![],
            );
            values.is_static = true;
            world.generating("Day", values).generating(
                "Entry",
                method("Entry", "date", class("java.lang.String"), vec![]),
            )
        };
        let walk = |name: &str, world: &World, statics: bool| {
            let ty = crate::resolve_written(name, &files, world, source.len() as u32 - 1);
            let walk = walk_members(&ty, &files, world, statics);
            let mut names: Vec<String> = walk
                .members
                .iter()
                .map(|member| member.name().to_owned())
                .collect();
            names.sort();
            (names, walk.closed, walk.whole_but_object)
        };
        let with = world(true);
        assert_eq!(
            walk("Day", &with, true),
            (vec!["MON".into(), "values".into()], true, true)
        );
        assert_eq!(walk("Day", &with, false), (vec!["name".into()], true, true));
        assert_eq!(
            walk("Entry", &with, false),
            (vec!["date".into(), "date".into()], true, true)
        );
        let without = world(false);
        assert_eq!(
            walk("Day", &without, true),
            (vec!["MON".into()], false, false)
        );
        assert_eq!(
            walk("Day", &without, false),
            (vec!["name".into()], true, true)
        );
        assert_eq!(
            walk("Entry", &without, false),
            (vec!["date".into()], false, false)
        );
        assert_eq!(
            walk("Day", &World::default(), false),
            (vec![], false, false),
            "a world without `Enum`"
        );
    }

    #[test]
    fn a_type_offers_its_own_fields_and_methods() {
        let names = names_of(
            "class Box { int width; String label() { return null; } }",
            "Box",
            false,
        );
        assert_eq!(names, vec!["label", "width"]);
    }

    #[test]
    fn a_type_offers_the_members_of_a_supertype_declared_in_this_file() {
        let source = "class Base { int id; } class Box extends Base { int width; }";
        assert_eq!(names_of(source, "Box", false), vec!["id", "width"]);
    }

    #[test]
    fn an_interface_this_file_declares_contributes_its_members() {
        let source = "interface Named { String name(); } class Box implements Named { int w; }";
        assert_eq!(names_of(source, "Box", false), vec!["name", "w"]);
    }

    #[test]
    fn a_supertype_this_file_does_not_declare_contributes_nothing() {
        let source = "class Box extends ArrayList { int width; }";
        assert_eq!(names_of(source, "Box", false), vec!["width"]);
    }

    #[test]
    fn a_file_local_class_inherits_its_external_supertypes_members() {
        // The commonest shape in a real project: the receiver is the project's
        // own class and the member is the framework's. Its neighbour
        // `a_supertype_this_file_does_not_declare_contributes_nothing` runs
        // against `NoExternals`, where `ArrayList` types as unknown, so it
        // never reaches the external arm at all.
        let world = World::default().with(
            ARRAY_LIST,
            &[],
            vec![method(
                ARRAY_LIST,
                "size",
                ExternalType::Primitive(Primitive::Int),
                vec![],
            )],
        );
        assert_eq!(
            names_with(
                "class Box extends ArrayList { int width; }",
                "Box",
                &world,
                false
            ),
            vec!["size", "width"]
        );
        // The same fact through the supertype's own name, which is what says
        // the walk's crossing rather than the world is what this measures.
        assert_eq!(names_in(&external(ARRAY_LIST), &world, false), vec!["size"]);
    }

    #[test]
    fn an_external_supertype_reached_twice_contributes_its_members_once() {
        // A diamond onto one framework type, which is what an fqn-keyed guard
        // buys: without it `size` is offered twice and a completion list shows
        // the same method under one name in two rows.
        let world = World::default().with(
            ARRAY_LIST,
            &[],
            vec![method(
                ARRAY_LIST,
                "size",
                ExternalType::Primitive(Primitive::Int),
                vec![],
            )],
        );
        let source = "interface Sized extends ArrayList {} \
                      class Box extends ArrayList implements Sized {}";
        assert_eq!(names_with(source, "Box", &world, false), vec!["size"]);
    }

    #[test]
    fn a_member_the_local_class_declares_shadows_the_external_one_it_inherits() {
        let world = World::default().with(
            ARRAY_LIST,
            &[],
            vec![method(
                ARRAY_LIST,
                "size",
                ExternalType::Primitive(Primitive::Int),
                vec![],
            )],
        );
        let source = "class Box extends ArrayList { int size() { return 0; } }";
        let (syntax, scopes) = parts(source);
        let files = set(&syntax, &scopes);
        let ty = crate::resolve_written("Box", &files, &world, source.len() as u32 - 1);
        let members = members_of(&ty, &files, &world, false);
        // Both are in the list, so it is the shadow rule and not the walk that
        // decides — the same premise the file-local shadow tests rest on.
        assert_eq!(members.len(), 2, "the override and the fact it overrides");
        let fact = members
            .iter()
            .find(|member| member.external().is_some())
            .expect("the inherited fact");
        assert!(
            !fact.declared_by(&ty),
            "a fact reached through a local subtype is inherited by construction"
        );

        let found = crate::infer::sole_candidate(
            &ty,
            "size",
            crate::infer::Access::Call { arguments: &[] },
            &files,
            &world,
        )
        .expect("the override wins rather than the pair being read as ambiguous");
        assert!(
            found.local_decl().is_some(),
            "the declaration the file holds, which is the only one with a span"
        );
    }

    #[test]
    fn an_external_interfaces_static_method_is_not_offered_on_a_local_implementer() {
        const NAMED: &str = "ext.Named";
        const BASE: &str = "ext.Base";
        let mut of = method(NAMED, "of", class(NAMED), vec![]);
        of.is_static = true;
        let mut max = field(NAMED, "MAX", ExternalType::Primitive(Primitive::Int));
        max.is_static = true;
        let mut create = method(BASE, "create", class(BASE), vec![]);
        create.is_static = true;
        let world = World::default()
            .with(NAMED, &[], vec![of, max])
            .with(BASE, &[], vec![create]);

        // JLS 8.4.8: the constant is inherited and the static method is not.
        assert_eq!(
            names_with("class Box implements Named {}", "Box", &world, true),
            vec!["MAX"]
        );
        // Which is a rule about the *interface* position and not about facts
        // being unreachable — a class supertype's static method is inherited,
        // because Java inherits that one. `implements` against `extends` is
        // the whole of the difference, and it is drawn on two types that could
        // each be what they are written as: a class extended and an interface
        // implemented, both of them Java that compiles.
        assert_eq!(
            names_with("class Box extends Base {}", "Box", &world, true),
            vec!["create"]
        );
        // And a receiver naming the interface itself still selects its static
        // method: it is not a supertype of itself.
        assert_eq!(names_in(&external(NAMED), &world, true), vec!["MAX", "of"]);
    }

    #[test]
    fn a_static_member_of_an_external_supertype_is_not_offered_to_a_value_receiver() {
        // `statics` filters the inherited half exactly as it filters the local
        // one, which is what stops `box.CASE_INSENSITIVE_ORDER` being offered.
        let source = "class Box extends String {}";
        assert_eq!(
            names_with(source, "Box", &split_world(), false),
            vec!["length"]
        );
        assert_eq!(
            names_with(source, "Box", &split_world(), true),
            vec!["CASE_INSENSITIVE_ORDER"]
        );
    }

    #[test]
    fn an_external_supertype_closure_is_charged_against_the_same_walk_budget() {
        // Three shapes, because the budget is checked in two places and each
        // check has two terms, and only all three together say the two halves
        // draw on one purse rather than each respecting a limit of its own.
        //
        // First: a local chain long enough to spend it leaves nothing for the
        // external supertype at its end, so a deep hierarchy cannot be
        // extended by asking the world.
        let world = World::default().with(
            ARRAY_LIST,
            &[],
            vec![method(
                ARRAY_LIST,
                "size",
                ExternalType::Primitive(Primitive::Int),
                vec![],
            )],
        );
        // The chain is exactly the budget long, so its last frame is visited
        // and its external supertype is what the budget refuses.
        let last = MAX_SUPERTYPES - 1;
        let mut deep = String::new();
        for level in 0..last {
            deep.push_str(&format!("class L{level} extends L{} {{}}\n", level + 1));
        }
        deep.push_str(&format!("class L{last} extends ArrayList {{}}\n"));
        assert!(names_with(&deep, "L0", &world, false).is_empty());
        // Shallow, so the same source shape with the same world does answer:
        // it is the budget and not the crossing that refused above.
        let shallow = "class L0 extends L1 {} class L1 extends ArrayList {}";
        assert_eq!(names_with(shallow, "L0", &world, false), vec!["size"]);

        // Second: breadth on the external side is charged too, so a class
        // implementing more framework interfaces than the budget allows is
        // bounded by the same number a wide file-local hierarchy is. One frame
        // — the class itself — is already spent when the loop starts.
        let mut wide = World::default();
        let mut implemented = Vec::new();
        for index in 0..MAX_SUPERTYPES + 6 {
            let fqn = format!("ext.I{index}");
            let member = method(&fqn, &format!("m{index}"), ExternalType::Void, vec![]);
            wide = wide.with(&fqn, &[], vec![member]);
            implemented.push(format!("I{index}"));
        }
        let source = format!("class Box implements {} {{}}", implemented.join(", "));
        assert_eq!(
            names_with(&source, "Box", &wide, false).len(),
            MAX_SUPERTYPES - 1
        );

        // Third, and the only one that pins the *frame* guard's external term:
        // a local frame queued before the asks were spent and popped after
        // them. The superclass is queued while the budget is untouched, the
        // interfaces then spend all of it, and `Local`'s own member is gone by
        // the time its frame comes off the queue — so a walk that stopped
        // counting asks when it checked a frame would answer `fromLocal` here.
        let wide_with_a_local = format!(
            "class Local {{ int fromLocal; }} class Box extends Local implements {} {{}}",
            implemented.join(", ")
        );
        let truncated = names_with(&wide_with_a_local, "Box", &wide, false);
        assert!(!truncated.contains(&"fromLocal".to_owned()));
        assert_eq!(truncated.len(), MAX_SUPERTYPES - 1);
        // Not vacuous: the identical shape with few enough asks to leave the
        // budget standing does reach the local frame.
        let narrow = "class Local { int fromLocal; } class Box extends Local implements I0 {}";
        assert_eq!(
            names_with(narrow, "Box", &wide, false),
            vec!["fromLocal", "m0"]
        );
    }

    #[test]
    fn a_supertype_that_resolves_to_nothing_still_contributes_nothing() {
        // Two ways to resolve to nothing, and the second is the invariant: a
        // name two held types answer to is ambiguous, and ambiguity answers
        // nothing rather than either candidate's members.
        let unknown = World::default().with(
            ARRAY_LIST,
            &[],
            vec![method(ARRAY_LIST, "size", ExternalType::Void, vec![])],
        );
        assert_eq!(
            names_with(
                "class Box extends Nowhere { int width; }",
                "Box",
                &unknown,
                false
            ),
            vec!["width"]
        );

        let ambiguous = World::default()
            .with(
                "one.Dup",
                &[],
                vec![method("one.Dup", "fromOne", ExternalType::Void, vec![])],
            )
            .with(
                "two.Dup",
                &[],
                vec![method("two.Dup", "fromTwo", ExternalType::Void, vec![])],
            );
        assert_eq!(
            names_with(
                "class Box extends Dup { int width; }",
                "Box",
                &ambiguous,
                false
            ),
            vec!["width"]
        );
    }

    #[test]
    fn an_inherited_external_members_type_resolves_the_same_way_a_receivers_does() {
        // One member, reached two ways, and the same answer both times: a
        // `Member` carries where it came from and nothing about how it was
        // reached, so `member_type` cannot tell the two apart.
        let world = World::default().with(
            STRING,
            &[],
            vec![method(STRING, "trim", class(STRING), vec![])],
        );
        let source = "class Box extends String {}";
        let (syntax, scopes) = parts(source);
        let files = set(&syntax, &scopes);
        let ty = crate::resolve_written("Box", &files, &world, source.len() as u32 - 1);
        let members = members_of(&ty, &files, &world, false);
        let [inherited] = members.as_slice() else {
            panic!("expected the one inherited member, got {members:?}");
        };
        assert_eq!(
            member_type(inherited, &files, &world),
            typed(&world, "trim"),
            "reached through a local subtype or by its own name is one type"
        );
        // And no span to navigate to, which is what keeps go-to-definition
        // from claiming to open the JDK's sources.
        assert!(inherited.file().is_none());
        assert!(inherited.local_decl().is_none());
    }

    #[test]
    fn a_supertype_cycle_terminates_instead_of_hanging() {
        // Invalid Java, entirely valid input to a parser that never rejects.
        let source = "class A extends B { int a; } class B extends A { int b; }";
        let names = names_of(source, "A", false);
        assert!(names.contains(&"a".to_string()));
        assert!(names.contains(&"b".to_string()));
    }

    #[test]
    fn static_and_instance_members_are_offered_separately() {
        let source = "class Box { static int COUNT; int width; }";
        assert_eq!(names_of(source, "Box", true), vec!["COUNT"]);
        assert_eq!(names_of(source, "Box", false), vec!["width"]);
    }

    #[test]
    fn a_nested_type_is_a_static_member_of_its_outer_type() {
        let source = "class Outer { class Inner {} }";
        assert_eq!(names_of(source, "Outer", true), vec!["Inner"]);
    }

    #[test]
    fn a_type_argument_is_substituted_into_an_inherited_members_type() {
        let source = "class Holder<T> { T value() { return null; } } \
                      class Boxes {} class Shelf extends Holder<Boxes> {}";
        let (syntax, scopes) = parts(source);
        let at = source.len() as u32 - 1;
        let ty = crate::resolve_written("Shelf", &set(&syntax, &scopes), &NoExternals, at);
        let members = members_of(&ty, &set(&syntax, &scopes), &NoExternals, false);
        let value = members
            .iter()
            .find(|member| member.name() == "value")
            .expect("the inherited method");
        let value_ty = member_type(value, &set(&syntax, &scopes), &NoExternals);
        let Type::Declared { name, .. } = &value_ty else {
            panic!("expected T substituted to Boxes, got {value_ty:?}");
        };
        assert_eq!(name, "Boxes");
    }

    #[test]
    fn an_unknown_type_has_no_members() {
        let (syntax, scopes) = parts("class A {}");
        let unknown = Type::Unknown("String".into());
        assert!(members_of(&unknown, &set(&syntax, &scopes), &NoExternals, false).is_empty());
    }

    #[test]
    fn an_empty_world_answers_no_external_members_and_every_answer_is_phase_2s() {
        // Two empty worlds, because they are empty in different ways:
        // `NoExternals` answers `None` by construction, and a hand-built world
        // with no types answers `None` for want of a fact. Neither may produce
        // a member, and neither may change what a file-local receiver answers.
        let string = external(STRING);
        for world in [&NoExternals as &dyn Externals, &World::default()] {
            assert!(names_in(&string, world, false).is_empty());
            assert!(names_in(&string, world, true).is_empty());
        }
        assert_eq!(
            names_of("class Box { int width; }", "Box", false),
            vec!["width"]
        );
    }

    #[test]
    fn a_fact_of_a_kind_a_dot_cannot_select_is_dropped_rather_than_offered() {
        // The trait owes this list, and a world that breaks the obligation
        // must not have a constructor rendered into a completion popup —
        // where it would appear without its parentheses, since only a
        // `DeclKind::Method` renders any. A nested type is dropped for the
        // other reason: it is a type fact of its own, reached by its own name.
        let mut constructor = method(STRING, "String", ExternalType::Void, vec![]);
        constructor.kind = DeclKind::Constructor;
        let mut nested = field(STRING, "CaseInsensitiveComparator", class(STRING));
        nested.kind = DeclKind::Class;
        let world = World::default().with(
            STRING,
            &[],
            vec![
                constructor,
                nested,
                method(
                    STRING,
                    "length",
                    ExternalType::Primitive(Primitive::Int),
                    vec![],
                ),
            ],
        );
        // The one admissible fact of the three, in both halves — the nested
        // type would be static if it were offered at all.
        assert_eq!(names_in(&external(STRING), &world, false), vec!["length"]);
        assert!(names_in(&external(STRING), &world, true).is_empty());
    }

    #[test]
    fn an_inherited_external_member_is_offered_alongside_a_declared_one() {
        // Inherited on this side of the door means one thing only: a fact
        // whose owner is not the type asked about. The closure was walked by
        // whatever answered, so `members_of` reads both from one list.
        let world = World::default().with(
            STRING,
            &[OBJECT],
            vec![
                method(
                    STRING,
                    "length",
                    ExternalType::Primitive(Primitive::Int),
                    vec![],
                ),
                method(OBJECT, "toString", class(STRING), vec![]),
            ],
        );
        let string = external(STRING);
        assert_eq!(names_in(&string, &world, false), vec!["length", "toString"]);

        let (syntax, scopes) = parts("class A {}");
        let members = members_of(&string, &set(&syntax, &scopes), &world, false);
        let inherited = members
            .iter()
            .find(|member| member.name() == "toString")
            .expect("the inherited method");
        // The owner is what own-shadows-inherited compares, so losing it is
        // the difference between a refusal and a wrong pick.
        assert_eq!(inherited.external().expect("a fact").owner, OBJECT);
        assert!(!inherited.declared_by(&string));
        let own = members
            .iter()
            .find(|member| member.name() == "length")
            .expect("the type's own method");
        assert!(own.declared_by(&string));
    }

    #[test]
    fn a_static_external_member_is_not_offered_to_a_value_receiver() {
        assert_eq!(
            names_in(&external(STRING), &split_world(), false),
            vec!["length"]
        );
    }

    #[test]
    fn an_instance_external_member_is_not_offered_to_a_type_name_receiver() {
        assert_eq!(
            names_in(&external(STRING), &split_world(), true),
            vec!["CASE_INSENSITIVE_ORDER"]
        );
    }

    #[test]
    fn an_external_members_return_type_is_the_type_the_facts_name() {
        let world = World::default().with(
            STRING,
            &[],
            vec![
                method(
                    STRING,
                    "length",
                    ExternalType::Primitive(Primitive::Int),
                    vec![],
                ),
                method(STRING, "trim", class(STRING), vec![]),
                method(STRING, "wait", ExternalType::Void, vec![]),
            ],
        );
        assert_eq!(
            typed(&world, "length"),
            Type::Primitive(Primitive::Int),
            "a primitive fact is a primitive type"
        );
        // Simple name and fully-qualified name both, because the simple one is
        // what a hover renders and the fqn is what the world is asked about
        // next — a chain through this type depends on the second being right.
        assert_eq!(
            typed(&world, "trim"),
            Type::External {
                name: "String".into(),
                fqn: STRING.into(),
                arguments: Vec::new(),
            }
        );
        assert_eq!(typed(&world, "wait"), Type::Void);
    }

    #[test]
    fn an_external_member_whose_type_nothing_resolved_types_as_unknown() {
        // All three shapes an unresolved fact arrives in: a generic nothing
        // settled, which keeps its arguments; a plain name; and the empty
        // string a member that writes no type at all carries, which is a
        // value and not an absence.
        let world = World::default().with(
            STRING,
            &[],
            vec![
                method(
                    STRING,
                    "nowhere",
                    ExternalType::Unresolved("Nowhere<String>".into()),
                    vec![],
                ),
                method(
                    STRING,
                    "variable",
                    ExternalType::Unresolved("E".into()),
                    vec![],
                ),
                field(STRING, "untyped", ExternalType::Unresolved(String::new())),
            ],
        );
        assert_eq!(
            typed(&world, "nowhere"),
            Type::Unknown("Nowhere<String>".into())
        );
        assert_eq!(typed(&world, "variable"), Type::Unknown("E".into()));
        assert_eq!(typed(&world, "untyped"), Type::Unknown(String::new()));
        // And unknown is what stops the chain, rather than an empty member
        // list that would read as "this type has none".
        assert!(!typed(&world, "untyped").is_known());
    }

    const LIST: &str = "java.util.List";
    const MAP: &str = "java.util.Map";

    fn variable(name: &str) -> ExternalType {
        ExternalType::Variable(name.to_owned())
    }

    fn parameterized(fqn: &str, arguments: Vec<ExternalType>) -> ExternalType {
        ExternalType::Parameterized(fqn.to_owned(), arguments)
    }

    fn external_with(fqn: &str, arguments: Vec<Type>) -> Type {
        Type::External {
            name: last_segment(fqn).to_owned(),
            fqn: fqn.to_owned(),
            arguments,
        }
    }

    /// `java.util.List<E>` with the shapes substitution has to get right: a
    /// variable of the type, one inside an argument, and a method's own.
    fn generic_world() -> World {
        World::default()
            .with(STRING, &[], vec![])
            .with(
                LIST,
                &[],
                vec![
                    method(
                        LIST,
                        "get",
                        variable("E"),
                        vec![ExternalType::Primitive(Primitive::Int)],
                    ),
                    method(
                        LIST,
                        "subList",
                        parameterized(LIST, vec![variable("E")]),
                        vec![],
                    ),
                    method(
                        LIST,
                        "toArray",
                        ExternalType::Unresolved("T[]".into()),
                        vec![],
                    ),
                    method(
                        LIST,
                        "add",
                        ExternalType::Primitive(Primitive::Boolean),
                        vec![variable("E")],
                    ),
                ],
            )
            .generic(&["E"])
            .with(
                MAP,
                &[],
                vec![
                    method(MAP, "get", variable("V"), vec![class(OBJECT)]),
                    method(MAP, "anyKey", variable("K"), vec![]),
                ],
            )
            .generic(&["K", "V"])
    }

    /// The type of `receiver`'s member called `name`.
    fn typed_on(receiver: &Type, world: &dyn Externals, name: &str) -> Type {
        let (syntax, scopes) = parts("class A {}");
        let files = set(&syntax, &scopes);
        let members = members_of(receiver, &files, world, false);
        let member = members
            .iter()
            .find(|member| member.name() == name)
            .unwrap_or_else(|| panic!("no member called {name}"));
        member_type(member, &files, world)
    }

    #[test]
    fn a_receivers_type_argument_replaces_the_variable_its_members_name() {
        let world = generic_world();
        let strings = external_with(LIST, vec![external(STRING)]);
        assert_eq!(typed_on(&strings, &world, "get"), external(STRING));
        assert_eq!(
            typed_on(&strings, &world, "subList"),
            external_with(LIST, vec![external(STRING)]),
            "inside an argument too"
        );
        // A method's own `T` is not the type's, and nothing binds it.
        assert_eq!(
            typed_on(&strings, &world, "toArray"),
            Type::Unknown("T[]".into())
        );
        // A parameter is substituted the same way, which is what overload
        // elimination compares an argument against.
        let (syntax, scopes) = parts("class A {}");
        let files = set(&syntax, &scopes);
        let add = members_of(&strings, &files, &world, false)
            .into_iter()
            .find(|member| member.name() == "add")
            .expect("add");
        assert_eq!(
            add.parameter_types(&files, &world),
            Some(vec![external(STRING)])
        );
    }

    /// Never `Object`, never a guess: a raw receiver, and one with the wrong
    /// number of arguments, leave every variable unknown — Java's erasure of
    /// `E` is `Object`, which would exclude `log(String)` at
    /// `log(raw.get(0))` where Java's unchecked call picks it.
    #[test]
    fn a_variable_nothing_binds_is_unknown_and_never_its_erasure() {
        let world = generic_world();
        assert_eq!(
            typed_on(&external(LIST), &world, "get"),
            Type::Unknown("E".into())
        );
        let short = external_with(MAP, vec![external(STRING)]);
        assert_eq!(typed_on(&short, &world, "get"), Type::Unknown("V".into()));
        assert_eq!(
            typed_on(&short, &world, "anyKey"),
            Type::Unknown("K".into()),
            "a short argument list binds nothing, not its first parameter"
        );
        let whole = external_with(MAP, vec![external(STRING), external(LIST)]);
        assert_eq!(typed_on(&whole, &world, "get"), external(LIST));
    }

    /// JLS 4.8: a raw type's members are erased — `subList` is a raw
    /// `List`, and an argument a supertype edge wrote is gone too, so a raw
    /// `Names<T> implements List<String>` has a `get` javac types `Object`
    /// and this answers unknown, never `String`.
    #[test]
    fn a_raw_receivers_member_types_are_erased() {
        let world = generic_world();
        assert_eq!(typed_on(&external(LIST), &world, "subList"), external(LIST));
        let source = "import java.util.List;\nabstract class Names<T> implements List<String> {}\n";
        let (syntax, scopes) = parts(source);
        let files = set(&syntax, &scopes);
        let at = source.len() as u32 - 1;
        let names = crate::resolve_written("Names", &files, &world, at);
        let get = members_of(&names, &files, &world, false)
            .into_iter()
            .find(|member| member.name() == "get")
            .expect("get");
        assert!(!member_type(&get, &files, &world).is_known());
        let typed = crate::resolve_written("Names<Foo>", &files, &world, at);
        let get = members_of(&typed, &files, &world, false)
            .into_iter()
            .find(|member| member.name() == "get")
            .expect("get");
        assert_eq!(member_type(&get, &files, &world), external(STRING));
    }

    #[test]
    fn a_local_class_writing_arguments_on_an_external_supertype_inherits_them() {
        let world = generic_world();
        let source = "import java.util.List;\nabstract class Names implements List<String> {}\n";
        let (syntax, scopes) = parts(source);
        let files = set(&syntax, &scopes);
        let at = source.len() as u32 - 1;
        let names = crate::resolve_written("Names", &files, &world, at);
        let get = members_of(&names, &files, &world, false)
            .into_iter()
            .find(|member| member.name() == "get")
            .expect("get");
        assert_eq!(member_type(&get, &files, &world), external(STRING));
    }

    /// A field declared `List<T>` on `Holder<Foo>` is a `List<Foo>`: the
    /// owner's binding reaches inside an external type's arguments.
    #[test]
    fn an_owners_binding_reaches_inside_an_external_members_arguments() {
        let world = generic_world();
        let source = "import java.util.List;\nclass Foo {}\nclass Holder<T> { List<T> items; }\n";
        let (syntax, scopes) = parts(source);
        let files = set(&syntax, &scopes);
        let at = source.len() as u32 - 1;
        let holder = crate::resolve_written("Holder<Foo>", &files, &world, at);
        let foo = crate::resolve_written("Foo", &files, &world, at);
        let items = members_of(&holder, &files, &world, false)
            .into_iter()
            .find(|member| member.name() == "items")
            .expect("items");
        let items = member_type(&items, &files, &world);
        assert_eq!(items, external_with(LIST, vec![foo.clone()]));
        assert_eq!(typed_on(&items, &world, "get"), foo);
    }

    #[test]
    fn a_record_offers_its_components() {
        assert_eq!(
            names_of("record Point(int x, int y) {}", "Point", false),
            vec!["x", "y"]
        );
    }

    #[test]
    fn a_nested_type_is_typed_as_itself_rather_than_void() {
        let source = "class Outer { class Inner {} }";
        let (syntax, scopes) = parts(source);
        let at = source.len() as u32 - 1;
        let ty = crate::resolve_written("Outer", &set(&syntax, &scopes), &NoExternals, at);
        let members = members_of(&ty, &set(&syntax, &scopes), &NoExternals, true);
        let inner = members
            .iter()
            .find(|member| member.name() == "Inner")
            .expect("the nested type");
        let inner_ty = member_type(inner, &set(&syntax, &scopes), &NoExternals);
        let Type::Declared { name, .. } = &inner_ty else {
            panic!("expected Inner typed as itself, got {inner_ty:?}");
        };
        assert_eq!(name, "Inner");
    }

    #[test]
    fn an_enum_constant_is_typed_as_its_enum() {
        let source = "enum Colour { RED }";
        let (syntax, scopes) = parts(source);
        let at = source.len() as u32 - 1;
        let ty = crate::resolve_written("Colour", &set(&syntax, &scopes), &NoExternals, at);
        let members = members_of(&ty, &set(&syntax, &scopes), &NoExternals, true);
        let red = members
            .iter()
            .find(|member| member.name() == "RED")
            .expect("the enum constant");
        let red_ty = member_type(red, &set(&syntax, &scopes), &NoExternals);
        let Type::Declared { name, .. } = &red_ty else {
            panic!("expected RED typed as Colour, got {red_ty:?}");
        };
        assert_eq!(name, "Colour");
    }

    #[test]
    fn an_interface_constant_is_offered_to_a_static_receiver() {
        let source = "interface Cfg { int MAX = 3; }";
        assert_eq!(names_of(source, "Cfg", true), vec!["MAX"]);
        assert!(names_of(source, "Cfg", false).is_empty());
    }

    #[test]
    fn an_interfaces_static_method_is_not_inherited_by_an_implementer() {
        let source = "interface Named { static Named of() { return null; } int id(); } \
                      class Box implements Named {}";
        assert!(names_of(source, "Box", true).is_empty());
        assert_eq!(names_of(source, "Box", false), vec!["id"]);
    }

    #[test]
    fn an_interface_constant_is_inherited_by_an_implementer() {
        let source = "interface Cfg { int MAX = 3; } class Box implements Cfg {}";
        assert_eq!(names_of(source, "Box", true), vec!["MAX"]);
    }

    #[test]
    fn a_constructor_and_an_initialiser_are_offered_to_neither_receiver() {
        let source = "class Box { Box() {} static {} { } int w; }";
        assert!(names_of(source, "Box", true).is_empty());
        assert_eq!(names_of(source, "Box", false), vec!["w"]);
    }

    #[test]
    fn a_member_knows_which_declaration_it_came_from() {
        let source = "class Base { int id; } class Box extends Base { int width; }";
        let (syntax, scopes) = parts(source);
        let at = source.len() as u32 - 1;
        let ty = crate::resolve_written("Box", &set(&syntax, &scopes), &NoExternals, at);
        let members = members_of(&ty, &set(&syntax, &scopes), &NoExternals, false);
        let id = members
            .iter()
            .find(|member| member.name() == "id")
            .expect("the inherited field");
        assert_eq!(owner_of(id).name, "Base");
        let width = members
            .iter()
            .find(|member| member.name() == "width")
            .expect("the type's own field");
        assert_eq!(owner_of(width).name, "Box");
    }

    #[test]
    fn a_generic_supertype_using_the_owners_own_parameter_substitutes_through() {
        let source = "class Holder<T> { T value() { return null; } } \
                      class Boxes {} class Shelf<U> extends Holder<U> {}";
        let (syntax, scopes) = parts(source);
        let at = source.len() as u32 - 1;
        let ty = crate::resolve_written("Shelf<Boxes>", &set(&syntax, &scopes), &NoExternals, at);
        let members = members_of(&ty, &set(&syntax, &scopes), &NoExternals, false);
        let value = members
            .iter()
            .find(|member| member.name() == "value")
            .expect("the inherited method");
        let value_ty = member_type(value, &set(&syntax, &scopes), &NoExternals);
        let Type::Declared { name, .. } = &value_ty else {
            panic!("expected T substituted to Boxes, got {value_ty:?}");
        };
        assert_eq!(name, "Boxes");
    }

    #[test]
    fn a_members_own_type_parameter_is_not_substituted_by_its_owners() {
        let source = "class Holder<T> { <T> T get() { return null; } } class Boxes {}";
        let (syntax, scopes) = parts(source);
        let at = source.len() as u32 - 1;
        let ty = crate::resolve_written("Holder<Boxes>", &set(&syntax, &scopes), &NoExternals, at);
        let members = members_of(&ty, &set(&syntax, &scopes), &NoExternals, false);
        let get = members
            .iter()
            .find(|member| member.name() == "get")
            .expect("the method with its own type parameter");
        assert_eq!(
            member_type(get, &set(&syntax, &scopes), &NoExternals),
            Type::Variable("T".into())
        );
    }

    #[test]
    fn an_exploding_generic_chain_stops_instead_of_exhausting_memory() {
        // Each level's extends clause substitutes the previous level's
        // argument into itself twice, doubling the type tree per level.
        // Without a node budget this is billions of allocations by level 30.
        let mut source = String::from("class Pair<A, B> {}\n");
        let levels = 40;
        for i in 0..levels {
            if i + 1 < levels {
                source.push_str(&format!(
                    "class A{i}<T> extends A{}<Pair<T, T>> {{}}\n",
                    i + 1
                ));
            } else {
                source.push_str(&format!("class A{i}<T> {{}}\n"));
            }
        }
        let (syntax, scopes) = parts(&source);
        let at = source.len() as u32 - 1;
        let ty = crate::resolve_written("A0", &set(&syntax, &scopes), &NoExternals, at);
        // Liveness: this must return rather than hang or exhaust memory.
        // None of the classes in the chain declare any members.
        let members = members_of(&ty, &set(&syntax, &scopes), &NoExternals, false);
        assert!(members.is_empty());
    }

    #[test]
    fn a_supertype_in_another_file_contributes_its_members() {
        // Driven from a receiver whose `DeclSite` already names file B, which
        // is the shape the code supports: `resolve_written` asks the current
        // file and then `Externals` and never the set's other entries, so
        // file A cannot name `Child` at all. Everything after that is the
        // walk's own doing — it reads B's syntax for the frame, resolves
        // `extends Parent` in B, and reaches a declaration the current file
        // has never heard of.
        let a = parts("class A {}");
        let source_b = "class Parent { int p; } class Child extends Parent { int c; }";
        let b = parts(source_b);
        let child = declared_in(OTHER, &b, source_b, "Child");

        assert_eq!(
            member_names(&child, &two(&a, &b), &NoExternals, false),
            vec!["c", "p"]
        );
    }

    #[test]
    fn a_members_walk_resolves_another_files_supertype_against_that_files_own_declarations() {
        // Expressed with a same-named declaration rather than with imports:
        // two files can only disagree about what an *import* means through
        // `Externals`, and an external type contributes no members for this
        // walk to show. A declaration each file makes for itself is the same
        // disagreement in the one currency `members_of` can pay out in.
        //
        // Both files declare `Parent`, with different fields. The frame is
        // B's `Child`, so `extends Parent` must mean B's — resolving it in
        // the file the question was asked in would answer `fromA`, which is a
        // member of a type this receiver has nothing to do with.
        let source_a = "class Parent { int fromA; }";
        let a = parts(source_a);
        let source_b = "class Parent { int fromB; } class Child extends Parent {}";
        let b = parts(source_b);
        let child = declared_in(OTHER, &b, source_b, "Child");

        assert_eq!(
            member_names(&child, &two(&a, &b), &NoExternals, false),
            vec!["fromB"]
        );
        // Not vacuous: A's decoy is a real declaration with a real member,
        // reachable through the same walk when it is the one asked about.
        assert_eq!(names_of(source_a, "Parent", false), vec!["fromA"]);
    }

    #[test]
    fn a_cycle_across_two_files_terminates() {
        // A supertype *edge* cannot cross a file by name — see
        // `a_supertype_in_another_file_contributes_its_members` — so the cycle
        // is built the one way a walk can leave the frame's file on its own:
        // `extends T`, whose `T` substitution carries a site from wherever
        // the use bound it. Invalid Java, entirely valid input to a parser
        // that never rejects.
        //
        // The receiver is `Zb<Za<Zb>>`, so the walk visits B, then A, then B
        // again and the guard drops the repeat. The two declarations sit at
        // the *same* span in their two files — both sources are `class X` of
        // the same shape — so a guard keyed on the span alone would drop A's
        // frame as already seen and lose `a`.
        let source_a = "class Za<U> extends U { int a; }";
        let source_b = "class Zb<T> extends T { int b; }";
        let a = parts(source_a);
        let b = parts(source_b);
        let za = declared_in(FILE, &a, source_a, "Za");
        let zb = declared_in(OTHER, &b, source_b, "Zb");
        let Type::Declared { decl: site_a, .. } = &za else {
            panic!("Za resolves in A");
        };
        let Type::Declared { decl: site_b, .. } = &zb else {
            panic!("Zb resolves in B");
        };
        assert_eq!(
            site_a.span, site_b.span,
            "the premise: one span, two files, two declarations"
        );

        let za_of_zb = Type::Declared {
            name: "Za".into(),
            decl: site_a.clone(),
            arguments: vec![zb.clone()],
        };
        let receiver = Type::Declared {
            name: "Zb".into(),
            decl: site_b.clone(),
            arguments: vec![za_of_zb],
        };

        // Liveness first: this must return rather than loop. Both frames'
        // members are present, which is what the `DeclSite` key buys.
        assert_eq!(
            member_names(&receiver, &two(&a, &b), &NoExternals, false),
            vec!["a", "b"]
        );
    }

    #[test]
    fn a_file_the_set_does_not_hold_answers_nothing() {
        // The same span, said of a file nothing can reach. Falling back to
        // the current file would answer `width` — a member of a type the
        // receiver is not, read out of a file it does not live in.
        let source = "class Box { int width; }";
        let views = parts(source);
        let box_in_a = declared_in(FILE, &views, source, "Box");

        assert!(
            members_of(
                &resited(&box_in_a, ABSENT),
                &set(&views.0, &views.1),
                &NoExternals,
                false
            )
            .is_empty()
        );
        // Not vacuous: the identical span in the file the set does hold
        // answers.
        assert_eq!(
            member_names(&box_in_a, &set(&views.0, &views.1), &NoExternals, false),
            vec!["width"]
        );
    }
}
