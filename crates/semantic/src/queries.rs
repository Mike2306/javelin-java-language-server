//! The questions a member access answers: the three a `receiver.` asks, and
//! the one an unqualified `name(…)` asks of its enclosing type.
//!
//! Two of that three *pick* a member — where a declared one is written, and
//! what a classpath-held one says — and they pick it in one place,
//! [`selected_member`], rather than each deciding for itself. They differ only
//! in what they can render from what it found, which is the whole reason there
//! are two: a declaration has a span and a buffer to slice, and a fact has
//! neither.
//!
//! All four return `None` when the cursor is not on such an access at all, so
//! the caller can fall back to the file-local queries in `javelin-parser`.
//! `Some` with nothing in it is a different answer: the cursor *is* after a
//! `.`, and there is nothing to say about the receiver. Offering every local in
//! scope there is the one thing completion must not do.

use javelin_document::TextDocument;
use javelin_parser::{
    Candidate, Definition, DefinitionKind, ScopeTree, Selection, SelectionKind, Span, SyntaxTree,
    member_selection_at, signature_of, unqualified_call_at,
};

use crate::externals::{ExternalSite, Externals};
use crate::files::{FileId, Files};
use crate::infer::{Access, call_owner, sole_candidate, type_of};
use crate::members::{Member, MemberWalk, walk_members};
use crate::types::Type;

/// Same cap as `javelin_parser::completions_at`, for the same reason: a
/// generated god-class has more members than a popup can show, and the list is
/// returned incomplete so the client re-asks as the prefix narrows.
const MAX_CANDIDATES: usize = 500;

/// What can be completed after `receiver.`, filtered by what has been typed so
/// far.
///
/// `None` means the cursor is not on a member selection at all — the caller
/// falls back to `javelin_parser::completions_at`. An empty
/// [`MemberCompletions::candidates`] means it is one, and either the
/// receiver's type is not something this query can see or nothing it exposes
/// matches the prefix; offering every local in scope there would answer a
/// question nobody asked.
pub fn member_completions_at(
    document: &TextDocument,
    tree: &SyntaxTree,
    files: &Files<'_>,
    externals: &dyn Externals,
    offset: u32,
) -> Option<MemberCompletions> {
    let selection = member_selection_at(document, tree, offset)?;
    let found = visible_members(&selection, files, externals);
    let candidates: Vec<Candidate> = found
        .members
        .into_iter()
        .filter(|member| member.name().starts_with(&selection.prefix))
        .take(MAX_CANDIDATES)
        .map(|member| candidate(document, tree, files.current(), &member))
        .collect();
    Some(MemberCompletions {
        // `<`, not `<=`: at exactly the cap nothing can be shown about whether
        // the next candidate existed, and an unproven claim is not one this
        // crate makes.
        complete: found.complete && candidates.len() < MAX_CANDIDATES,
        candidates,
    })
}

/// What can be completed after a `.`, and whether that is the whole of it.
///
/// `complete` is a *proof*, and its absence is not a complaint: everything in
/// `candidates` is offerable either way, and the flag only says whether a
/// caller may claim there is nothing else. Two things can withhold it. One is
/// a list cut off at `MAX_CANDIDATES`. The other is anything the walk stepped
/// past — a receiver whose type nothing settled, a supertype that did not
/// resolve, one nobody wrote down (`extends java.lang.Object`, and the enum's
/// and record's implicit supertypes), a file the query cannot reach, an
/// exhausted budget, an external closure the world reports short, or a static
/// list on an external type, whose nested types are facts of their own and not
/// members. `members::walk_members` is where each of those is recorded.
///
/// An unknown receiver needs no term of its own: nothing walks one, so the
/// list is empty and `complete` is already `false` — the empty list is a
/// refusal there, and marking it whole would say "nothing completes here" of a
/// position this crate simply could not read.
#[derive(Debug, Clone)]
pub struct MemberCompletions {
    pub candidates: Vec<Candidate>,
    /// Whether `candidates` is everything the receiver's type exposes under
    /// the typed prefix. `false` means it may be short.
    pub complete: bool,
}

/// Where a member selected at `receiver.name` is declared.
///
/// `None` means either that the cursor is not on a member selection, or that
/// it is one but the member could not be resolved — an unknown receiver, a
/// name the receiver's type does not expose, or one it exposes twice over
/// (two overloads neither the arity nor the argument types can separate).
/// `sole_candidate` decides, so this answers the same as an unqualified call
/// in `call_definition_at` — one filter, not two that could disagree.
/// `member_completions_at`'s `.` is a third path but not a third vote: it
/// lists rather than picks, and never reaches `sole_candidate` at all.
///
/// `sole_candidate` searches both the static and instance namespaces, so this
/// loses the split `visible_members` gives completion; that is
/// `sole_candidate`'s own documented behaviour (its `ponytail:` comment says
/// so) and only misfires on Java that would not compile.
pub fn member_definition_at(
    document: &TextDocument,
    tree: &SyntaxTree,
    files: &Files<'_>,
    externals: &dyn Externals,
    offset: u32,
) -> Option<Definition> {
    let (reference, found) = selected_member(document, tree, files, externals, offset)
        .or_else(|| annotation_element(document, tree, files, externals, offset))
        .or_else(|| imported_value(document, tree, files, externals, offset))
        .or_else(|| case_constant(document, tree, files, externals, offset))?;
    // Everything below reads the *requested* document: `signature_of` slices
    // its text, and the buffer to slice for a member another file declares is
    // not here. `Definition` now carries a file, so the answer being nothing
    // is not that there is nowhere to put it — it is that `signature_of` has
    // nothing to slice.
    //
    // ponytail: this refuses a member another file in the set declares. No
    // set the server builds has one — a sibling file's member arrives as an
    // external fact, and `external_member_at` answers it from the index's
    // range. The upgrade, if a multi-file set is ever built, is `Files`
    // carrying a document per entry, so such a member has a buffer to slice
    // a signature out of.
    (found.file() == Some(files.current())).then_some(())?;
    let decl = found.local_decl()?;
    Some(Definition {
        file: files.current().clone(),
        reference,
        name: decl.name.clone(),
        kind: DefinitionKind::from(decl.kind),
        span: decl.span,
        name_span: decl.name_span,
        signature: signature_of(document, tree, decl.span),
        doc: decl.doc.clone(),
    })
}

/// What an external member selected at `receiver.name` is, rendered from
/// facts.
///
/// [`member_definition_at`]'s sibling rather than its rival: both ask the same
/// private `selected_member` which member the `.` selects, and then one of
/// them can render what came back and the other cannot. A declaration has a span in the
/// requested document and text to slice a signature out of; a classpath fact
/// has neither, so a [`Definition`] cannot carry one and manufacturing its
/// spans would render a foreign declaration against the open buffer.
///
/// `None` when the cursor is not on a member selection, when nothing
/// resolved, or when what resolved is a member some reachable file declares —
/// that last one is [`member_definition_at`]'s answer and deliberately not
/// this one's, so a caller that tries both in either order gets one answer and
/// never two.
pub fn external_member_at(
    document: &TextDocument,
    tree: &SyntaxTree,
    files: &Files<'_>,
    externals: &dyn Externals,
    offset: u32,
) -> Option<ExternalSummary> {
    // A `.` or, failing that, a call with no receiver — the member an
    // inherited `greet()` names is as external as the one `this.greet()`
    // selects. The two shapes cannot both be under one cursor, so the
    // fall-through is not a preference.
    let (reference, found) = selected_member(document, tree, files, externals, offset)
        .or_else(|| called_member(document, tree, files, externals, offset))
        .or_else(|| imported_value(document, tree, files, externals, offset))
        .or_else(|| annotation_element(document, tree, files, externals, offset))
        .or_else(|| case_constant(document, tree, files, externals, offset))?;
    Some(summary(reference, found.external()?))
}

fn summary(reference: Span, facts: &crate::ExternalMember) -> ExternalSummary {
    ExternalSummary {
        reference,
        name: facts.name.clone(),
        kind: DefinitionKind::from(facts.kind),
        signature: facts.signature(),
        owner: facts.owner.clone(),
        ordinal: facts.ordinal,
        site: facts.site.clone(),
        parameter_names: if facts.parameter_names.len() == facts.parameters.len() {
            facts.parameter_names.clone()
        } else {
            Vec::new()
        },
    }
}

/// One external member, as much of it as facts can say.
///
/// Not a [`Definition`], and the missing fields are the reason this type
/// exists: there is no declaration span, no name span and no doc comment,
/// because nothing about this member was ever text in a file the request
/// named. `signature` is [`crate::ExternalMember::signature`]'s rendering —
/// simple names, and a method's parameter types without their names, whose
/// ceiling and upgrade are recorded there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalSummary {
    /// A method's parameters' names, in order — empty where the facts cannot
    /// name every one.
    pub parameter_names: Vec<String>,
    /// The identifier the question was asked about. The one span here, and
    /// safe for the reason the others are absent: it is the *use* site, which
    /// is in the buffer the request named.
    pub reference: Span,
    pub name: String,
    pub kind: DefinitionKind,
    /// The member as one signature line, `int length()`.
    pub signature: String,
    /// The fully-qualified name of the type that *declares* the member, which
    /// is not always the receiver's — an inherited member names the supertype
    /// it came from.
    ///
    /// With [`Self::ordinal`], the key references record a member under when
    /// it has no workspace site.
    pub owner: String,
    /// Copied through from [`crate::ExternalMember::ordinal`].
    pub ordinal: u32,
    /// Where the member is written down, copied through from
    /// [`crate::ExternalMember::site`] unchanged.
    ///
    /// The one field here that is *not* in the buffer the request named — the
    /// mirror image of [`Self::reference`]'s doc, which is safe for exactly
    /// the opposite reason: that one is the *use* site, in the requested
    /// buffer; this one is the *declaration* site, in whatever `source`
    /// names. A caller that renders this into a jump owes opening that file,
    /// not the one the request came from.
    pub site: Option<ExternalSite>,
}

/// What the `new T(…)` whose class name is at `offset` selects: one of
/// `T`'s own constructors, picked as a call picks a method, from a list shown
/// whole. Equally the `this(…)` or `super(…)` whose keyword is there, and —
/// with `constants` — the enum constant whose name is, which a caller asking
/// what the name *is* leaves out, since that is the constant.
///
/// Held to a type hint's rules, since the name names the type whatever the
/// constructor: [`SelectedConstructor::written`] is every name the pick read,
/// for a caller that refuses one a static import, an inherited member type or
/// a generated source may mean.
pub fn constructor_at(
    document: &TextDocument,
    tree: &SyntaxTree,
    files: &Files<'_>,
    externals: &dyn Externals,
    offset: u32,
    constants: bool,
) -> Option<SelectedConstructor> {
    let (reference, (found, written)) = match javelin_parser::creation_at(document, tree, offset) {
        Some((reference, expr)) => (
            reference,
            crate::infer::constructor_selected(&expr, files, externals)?,
        ),
        None => {
            let (reference, receiver, arguments) =
                javelin_parser::explicit_constructor_call_at(document, tree, offset, constants)?;
            (
                reference,
                crate::infer::explicit_constructor_selected(
                    &receiver, &arguments, files, externals,
                )?,
            )
        }
    };
    let parameter_names: Vec<String> = match (found.local_decl(), found.external()) {
        (Some(decl), _) => decl.parameters.iter().map(|it| it.name.clone()).collect(),
        (None, Some(facts)) if facts.parameter_names.len() == facts.parameters.len() => {
            facts.parameter_names.clone()
        }
        _ => Vec::new(),
    };
    let parameter_names = if parameter_names.iter().all(|name| !name.is_empty()) {
        parameter_names
    } else {
        Vec::new()
    };
    let target = match (found.local_decl(), found.external()) {
        (Some(decl), _) => ConstructorTarget::Local(Definition {
            file: files.current().clone(),
            reference,
            name: decl.name.clone(),
            kind: DefinitionKind::from(decl.kind),
            span: decl.span,
            name_span: decl.name_span,
            signature: signature_of(document, tree, decl.span),
            doc: decl.doc.clone(),
        }),
        (None, Some(facts)) => ConstructorTarget::External(summary(reference, facts)),
        _ => return None,
    };
    Some(SelectedConstructor {
        target,
        written,
        parameter_names,
    })
}

/// What [`constructor_at`] answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedConstructor {
    pub target: ConstructorTarget,
    pub written: Vec<String>,
    /// Its parameters' names, in order — empty where any is unknown.
    pub parameter_names: Vec<String>,
}

/// A constructor the open file declares, or one only facts describe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstructorTarget {
    Local(Definition),
    External(ExternalSummary),
}

/// What type the *name* at `offset` means, when no file this query can reach
/// declares it.
///
/// The type-level sibling of [`external_member_at`], and the first answer here
/// that is not about a member at all: `Greeter greeter;` in a file that
/// declares no `Greeter` is a question about a name, and [`Externals`] is
/// what turns it into a fully-qualified one — through the file's own package
/// and imports, so the answer is the one the compiler would reach and nothing
/// is picked by simple name.
///
/// `None` whenever the name settles anywhere nearer than [`Externals`]: a type
/// parameter, a type the file declares, a local the scope tree binds, an
/// ambiguity, or a position that is not a plain name at all. Everything in
/// that list already has an answer — [`javelin_parser::definition_at`]'s — and
/// a second one here would be a second vote on a question already settled.
///
/// Deliberately *not* a [`javelin_parser::Definition`], and for the same
/// reason [`ExternalSummary`] is not one: this crate has no file for the
/// declaration and no text to slice a signature out of. What it hands back is
/// a fully-qualified name, and a caller that can turn one into a location —
/// the server, which holds the workspace source index — does the rest.
pub fn external_type_at(
    document: &TextDocument,
    tree: &SyntaxTree,
    files: &Files<'_>,
    externals: &dyn Externals,
    offset: u32,
) -> Option<ExternalTypeRef> {
    let Some((name, reference)) = javelin_parser::name_at(document, tree, offset) else {
        // A segment of a qualified name, where only a type or a package can
        // be meant (JLS 6.5.4.1), so no value is asked: `java.util.List`,
        // `Map.Entry`, an import's.
        let (written, reference) = javelin_parser::qualified_name_at(document, tree, offset)?;
        // A first segment the scope tree binds as a type — a type parameter,
        // a local class — is one `resolve_written` would read past.
        let first = written.split('.').next()?;
        if files
            .current_scopes()
            .resolve_type(reference.start_byte, first)
            .is_some()
        {
            return None;
        }
        return match crate::resolve::resolve_written(
            &written,
            files,
            externals,
            reference.start_byte,
        ) {
            Type::External { fqn, .. } => Some(ExternalTypeRef {
                reference,
                fqn,
                written,
            }),
            _ => None,
        };
    };
    // A name the scope tree binds as a *value* is not a type reference, JLS
    // 6.5.2, and `resolve_written` would not see it: it asks the type
    // namespace alone, so a field called `value` beside a workspace type
    // called `value` would resolve to the type. Refusing here rather than
    // relying on a caller asking `definition_at` first keeps this answer safe
    // whatever order it is asked in — the ordering bug that shipped Phase 2a's
    // completion is the one this crate can least afford twice.
    //
    // ponytail: refuses a *type* written where only a type can go when a value
    // of that name is in scope — `value v;` beside a field `value`. A missing
    // answer, and only for source that ignores Java's naming conventions; the
    // upgrade is to ask `javelin_parser` whether the node was a
    // `type_identifier`, which is the distinction the grammar already makes.
    if files
        .current_scopes()
        .resolve_value(reference.start_byte, &name)
        .is_some()
    {
        return None;
    }
    // Resolved at the name's own offset, which is what decides the scopes it
    // is read against — the same rule every other caller of `resolve_written`
    // follows.
    match crate::resolve::resolve_written(&name, files, externals, reference.start_byte) {
        Type::External { fqn, .. } => Some(ExternalTypeRef {
            reference,
            fqn,
            written: name,
        }),
        _ => None,
    }
}

/// A name that meant a type no reachable file declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalTypeRef {
    /// The name as written, in the buffer the request named.
    pub reference: Span,
    /// What it resolved to, fully qualified. Where that type is written down
    /// is not this crate's to say: a JDK fact's span and a workspace type's
    /// place are the server's to look up.
    pub fqn: String,
    /// The text that was resolved: the name itself, or a qualified name's
    /// dotted text through the segment under the cursor.
    pub written: String,
}

/// The member a `receiver.name` at `offset` selects, and the span of the name
/// that selected it.
///
/// The one place a `.` decides *which* member it means.
/// [`member_definition_at`] and [`external_member_at`] both come through here,
/// so the selection rule, the namespace, the arity and argument filters and
/// the refusal are one vote rather than two that could disagree — which is the
/// same argument `sole_candidate` itself is there to settle one level down.
/// `member_completions_at`'s `.` is a third path and still not a third vote:
/// it lists rather than picks, and reaches neither this nor `sole_candidate`.
fn selected_member<'a>(
    document: &TextDocument,
    tree: &SyntaxTree,
    files: &Files<'a>,
    externals: &dyn Externals,
    offset: u32,
) -> Option<(Span, Member<'a>)> {
    let selection = member_selection_at(document, tree, offset)?;
    let reference = selection.name_span?;
    let owner = type_of(&selection.receiver, files, externals);
    let access = match &selection.kind {
        SelectionKind::Value => Access::Value,
        SelectionKind::MethodRef => Access::MethodRef,
        SelectionKind::Call { arguments } => Access::Call { arguments },
    };
    // `name`, not `prefix`: the cursor is usually inside the identifier rather
    // than at its end, and `b.wid|th` is a question about `width`.
    let found =
        sole_candidate(&owner, &selection.name, access, files, externals).or_else(|| {
            // `Access::Value` sees no methods, so `b.lab|el` — a call whose `(`
            // the user has not typed yet — would blink out mid-keystroke. Retry
            // once as `MethodRef`: it widens which *kind* is visible, never which
            // candidate wins, so two overloads still refuse — `MethodRef` carries
            // no arity to tell them apart.
            matches!(access, Access::Value)
                .then(|| {
                    sole_candidate(&owner, &selection.name, Access::MethodRef, files, externals)
                })
                .flatten()
        })?;
    Some((reference, found))
}

/// The element `@A(name = …)`'s `name` names: `A`'s `name()`, through the
/// same `sole_candidate` a call with no arguments goes through, with `A`
/// resolved where it is written — and nothing where `A` may be a type the
/// resolver does not see.
fn annotation_element<'a>(
    document: &TextDocument,
    tree: &SyntaxTree,
    files: &Files<'a>,
    externals: &dyn Externals,
    offset: u32,
) -> Option<(Span, Member<'a>)> {
    let element = javelin_parser::annotation_element_at(document, tree, offset)?;
    let first = element.annotation.split('.').next()?;
    if externals.may_shadow_type(first, files.current_syntax()) {
        return None;
    }
    let owner = crate::resolve::resolve_written(
        &element.annotation,
        files,
        externals,
        element.annotation_at,
    );
    let access = Access::Call { arguments: &[] };
    let found = sole_candidate(&owner, &element.name, access, files, externals)?;
    Some((element.name_span, found))
}

/// The enum constant a `case` label's simple name is: under a selector of an
/// enum type the name is that enum's constant, whatever the scope binds (JLS
/// 14.11.1). `None` where the selector's type has no enum constant of the
/// name, which leaves an ordinary constant expression to the scope.
fn case_constant<'a>(
    document: &TextDocument,
    tree: &SyntaxTree,
    files: &Files<'a>,
    externals: &dyn Externals,
    offset: u32,
) -> Option<(Span, Member<'a>)> {
    let (name, span, selector) = javelin_parser::case_constant_at(document, tree, offset)?;
    if crate::infer::root_may_be_another(&selector, files, externals) {
        return None;
    }
    let ty = type_of(&selector, files, externals);
    let found = sole_candidate(&ty, &name, Access::Value, files, externals)?;
    (found.kind() == javelin_parser::DeclKind::EnumConstant).then_some((span, found))
}

/// Whether the scope may say what the name at `offset` means: anywhere but a
/// `case` label, and there only under a selector of a primitive, its box or
/// `String` type, where a constant is an ordinary name. Under an enum the name
/// is the enum's constant, and under a type nothing settles yet it may be.
pub fn scope_answers_case_label(
    document: &TextDocument,
    tree: &SyntaxTree,
    files: &Files<'_>,
    externals: &dyn Externals,
    offset: u32,
) -> bool {
    let Some((_, _, selector)) = javelin_parser::case_constant_at(document, tree, offset) else {
        return true;
    };
    // A pattern variable the scope tree keeps past its `if` may be another
    // variable of another type, so neither reading is safe.
    if crate::infer::root_may_be_another(&selector, files, externals) {
        return false;
    }
    match type_of(&selector, files, externals) {
        Type::Primitive(_) => true,
        Type::External { fqn, .. } => matches!(
            fqn.as_str(),
            "java.lang.String"
                | "java.lang.Integer"
                | "java.lang.Character"
                | "java.lang.Short"
                | "java.lang.Byte"
        ),
        _ => false,
    }
}

/// Where the method an unqualified `name(…)` calls is declared.
///
/// `None` means the cursor is not on an unqualified call, or that it is one and
/// the enclosing type does not name exactly one method that fits — no such
/// method, a type this file does not declare, or two that both fit.
///
/// The *sole* candidate, not the first: this is `infer::sole_candidate`'s
/// refusal, the same one [`member_definition_at`] above answers through. A
/// qualified and an unqualified call used to differ here — the qualified path
/// took the first match — but both now go through one filter, so an
/// unqualified call refusing ambiguity and a qualified one accepting it is no
/// longer a distinction that exists. Argument types narrow which calls are
/// ambiguous at all; what remains ambiguous after them still answers nothing.
pub fn call_definition_at(
    document: &TextDocument,
    tree: &SyntaxTree,
    files: &Files<'_>,
    externals: &dyn Externals,
    offset: u32,
) -> Option<Definition> {
    let (name_span, found) = called_member(document, tree, files, externals, offset)?;
    // The same refusal, for the same reason and with the same ceiling, as
    // [`member_definition_at`]'s.
    (found.file() == Some(files.current())).then_some(())?;
    let decl = found.local_decl()?;
    Some(Definition {
        file: files.current().clone(),
        reference: name_span,
        name: decl.name.clone(),
        kind: DefinitionKind::from(decl.kind),
        span: decl.span,
        name_span: decl.name_span,
        signature: signature_of(document, tree, decl.span),
        doc: decl.doc.clone(),
    })
}

/// The names of the parameters of the method the call whose name is at
/// `offset` selects — `receiver.name(…)` or an unqualified `name(…)` — for a
/// caller that puts each beside its argument.
///
/// The pick is definition's, through the same `sole_candidate`, and then two
/// refusals definition does not make, because a hint is shown on every call
/// in view where a jump is asked for one: the receiver's member list must be
/// whole — a supertype nothing resolved could hold the overload Java picks —
/// short only by the `java.lang.Object` no declaration writes, whose own
/// method names are therefore refused; and every parameter must be named.
pub fn call_parameter_names_at(
    document: &TextDocument,
    tree: &SyntaxTree,
    files: &Files<'_>,
    externals: &dyn Externals,
    offset: u32,
) -> Option<Vec<String>> {
    let (owner, found) = match member_selection_at(document, tree, offset) {
        Some(selection) => {
            let SelectionKind::Call { arguments } = &selection.kind else {
                return None;
            };
            let owner = type_of(&selection.receiver, files, externals);
            let found = sole_candidate(
                &owner,
                &selection.name,
                Access::Call { arguments },
                files,
                externals,
            )?;
            (owner, found)
        }
        None => {
            let javelin_parser::Expr::Call {
                name,
                name_span,
                arguments,
                receiver: None,
                ..
            } = unqualified_call_at(document, tree, offset)?
            else {
                return None;
            };
            let owner = call_owner(files, externals, &name, name_span.start_byte);
            let found = sole_candidate(
                owner.ty(),
                &name,
                Access::Call {
                    arguments: &arguments,
                },
                files,
                externals,
            )
            .filter(|found| owner.admits(found))?;
            (owner.ty().clone(), found)
        }
    };
    if OBJECT_METHODS.contains(&found.name()) {
        return None;
    }
    let whole = [false, true]
        .into_iter()
        .all(|statics| walk_members(&owner, files, externals, statics).whole_but_object);
    if !whole {
        return None;
    }
    let names: Vec<String> = match (found.local_decl(), found.external()) {
        (Some(decl), _) => {
            if found.file() != Some(files.current()) {
                return None;
            }
            decl.parameters.iter().map(|it| it.name.clone()).collect()
        }
        (None, Some(facts)) if facts.parameter_names.len() == facts.parameters.len() => {
            facts.parameter_names.clone()
        }
        _ => return None,
    };
    names.iter().all(|name| !name.is_empty()).then_some(names)
}

/// What `java.lang.Object` declares, which no file-local member list holds.
pub(crate) const OBJECT_METHODS: [&str; 9] = [
    "equals",
    "hashCode",
    "toString",
    "getClass",
    "notify",
    "notifyAll",
    "wait",
    "clone",
    "finalize",
];

/// The member an unqualified `name(…)` at `offset` calls, and the span of the
/// name — [`selected_member`]'s sibling for a call with no receiver, answered
/// from the enclosing type through the same `sole_candidate`.
fn called_member<'a>(
    document: &TextDocument,
    tree: &SyntaxTree,
    files: &Files<'a>,
    externals: &dyn Externals,
    offset: u32,
) -> Option<(Span, Member<'a>)> {
    // `receiver: None`, spelled out rather than swallowed by `..`:
    // `unqualified_call_at` already guarantees it, and this is the one place
    // where losing that guarantee is a *wrong* jump rather than a missing one —
    // a qualified call resolved against the enclosing type instead of its
    // receiver's. The contract is cheaper to enforce at both ends than to trust
    // at one.
    let javelin_parser::Expr::Call {
        name,
        name_span,
        arguments,
        receiver: None,
        ..
    } = unqualified_call_at(document, tree, offset)?
    else {
        return None;
    };
    let owner = call_owner(files, externals, &name, name_span.start_byte);
    let found = sole_candidate(
        owner.ty(),
        &name,
        Access::Call {
            arguments: &arguments,
        },
        files,
        externals,
    )
    .filter(|found| owner.admits(found))?;
    Some((name_span, found))
}

/// The field or enum constant a bare name the file binds nothing of means:
/// an enclosing type's generated field, or failing every enclosing type one
/// a static import brings in.
fn imported_value<'a>(
    document: &TextDocument,
    tree: &SyntaxTree,
    files: &Files<'a>,
    externals: &dyn Externals,
    offset: u32,
) -> Option<(Span, Member<'a>)> {
    let (name, span) = javelin_parser::expression_name_at(document, tree, offset)?;
    if files
        .current_scopes()
        .resolve_value(span.start_byte, &name)
        .is_some()
    {
        return None;
    }
    if let Some((_, found)) = crate::infer::field_owner(files, externals, &name, span.start_byte) {
        return Some((span, found));
    }
    let owner = crate::imports::imported_owner(
        files,
        externals,
        &name,
        crate::imports::Namespace::Value,
        span.start_byte,
    )?;
    let found = sole_candidate(&owner, &name, Access::Value, files, externals)?;
    crate::imports::importable(&found).then_some((span, found))
}

/// The members the receiver of this selection exposes.
///
/// A receiver that names a type sees static members; one that holds a value
/// sees instance members. Which it is comes from the type the receiver typed
/// to and whether the name it used was bound in the variable namespace.
///
/// `walk_members` rather than `members_of`, because this is the one path that
/// *lists* rather than picks, and a list is the only answer whose caller can
/// say "and there is nothing else".
fn visible_members<'a>(
    selection: &Selection,
    files: &Files<'a>,
    externals: &dyn Externals,
) -> MemberWalk<'a> {
    let ty = type_of(&selection.receiver, files, externals);
    let statics = names_a_type(selection, files.current_scopes(), &ty);
    walk_members(&ty, files, externals, statics)
}

/// Whether the receiver names a type rather than a value.
///
/// Java also permits reaching a static member through a value (`obj.field`),
/// which this deliberately does not offer — the alternative fills the popup
/// with static noise on top of every instance member.
///
/// ponytail: static-through-value is simply not offered. The ceiling is
/// Java's, not this file's, and widening it means checking both namespaces
/// and merging the two member sets rather than choosing one.
///
/// The scope tree is asked first and its answers are final, because it is the
/// only thing that knows Java's two namespaces apart: a name it binds as a
/// *value* is a value even where a type of the same name exists, and reading
/// that the other way round would flip the split on file-local receivers as
/// well. `ty` is consulted only where the tree binds neither namespace, which
/// is where a classpath name lands — `ScopeTree` binds nothing an import
/// brings in, so `Integer.` reaches here bound in neither, and without this
/// last line it would be read as a value and offered instance members it
/// cannot reach while `parseInt` stayed invisible. A [`Type::External`] there
/// is the receiver having resolved to a *type*, which is the same
/// demonstration `resolve_type` gives for a local one.
fn names_a_type(selection: &Selection, scopes: &ScopeTree, ty: &Type) -> bool {
    let javelin_parser::Expr::Name { name, span } = &selection.receiver else {
        return false;
    };
    if scopes.resolve_value(span.start_byte, name).is_some() {
        return false;
    }
    if scopes.resolve_type(span.start_byte, name).is_some() {
        return true;
    }
    matches!(ty, Type::External { .. })
}

/// One completion item for a member, rendered against `document`.
///
/// A member's name and kind mean the same thing wherever it was declared, but
/// its signature is *text*, sliced out of the requested document — so a member
/// from another file is offered without one rather than with a slice of the
/// wrong buffer. Dropping the candidate instead would hide a name that is
/// correct to offer.
///
/// An external member is the one that has a detail without a buffer to slice:
/// nothing about it was ever text, so it is rendered from its facts instead.
fn candidate(
    document: &TextDocument,
    tree: &SyntaxTree,
    current: &FileId,
    member: &Member<'_>,
) -> Candidate {
    if let Some(facts) = member.external() {
        return Candidate {
            name: facts.name.clone(),
            kind: DefinitionKind::from(facts.kind),
            detail: Some(facts.signature()),
            // No comment travels with a type fact; see `Member::doc`.
            doc: None,
        };
    }
    Candidate {
        name: member.name().to_owned(),
        kind: DefinitionKind::from(member.kind()),
        // The same rule the file-local query uses: a declared type where there
        // is one, otherwise the declaration with its body left off, so two
        // methods of one name stay distinguishable.
        detail: match member.declared_type() {
            Some(declared) if member.kind() != javelin_parser::DeclKind::Method => {
                Some(declared.to_owned())
            }
            _ if member.file() != Some(current) => None,
            // Past that guard the member is in the requested file, so it has a
            // declaration to slice a signature out of.
            _ => member
                .local_decl()
                .and_then(|decl| non_empty(signature_of(document, tree, decl.span))),
        },
        doc: member.doc().map(str::to_owned),
    }
}

fn non_empty(text: String) -> Option<String> {
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoExternals;
    use crate::externals::testing::{World, field, method};
    use crate::externals::{ExternalMember, ExternalSite, ExternalType};
    use javelin_document::{DocumentId, TextDocument};
    use javelin_parser::{FileSyntax, JavaParser};

    /// The one file every fixture below is, named the same way its document
    /// is so that a `Files` set and a `TextDocument` agree on who they are.
    const FILE: &str = "file:///A.java";

    struct Fixture {
        document: TextDocument,
        tree: javelin_parser::SyntaxTree,
        syntax: FileSyntax,
        scopes: ScopeTree,
        offset: u32,
    }

    impl Fixture {
        fn files(&self) -> Files<'_> {
            Files::single(FileId::new(FILE), &self.syntax, &self.scopes)
        }
    }

    fn fixture(marked: &str) -> Fixture {
        let offset = marked.find('|').expect("mark the cursor with |") as u32;
        let source = marked.replace('|', "");
        let document = TextDocument::new(DocumentId::new(FILE), 1, source);
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let scopes = ScopeTree::build(&document, &tree);
        Fixture {
            document,
            tree,
            syntax,
            scopes,
            offset,
        }
    }

    /// A world holding `java.lang.String` and two of its members, in
    /// declaration order rather than sorted: a completion list keeps the order
    /// the walk produced, which is what the assertions below read.
    fn string_world() -> World {
        let int = || ExternalType::Primitive(crate::Primitive::Int);
        World::default().with(
            "java.lang.String",
            &[],
            vec![
                method("java.lang.String", "length", int(), vec![]),
                method(
                    "java.lang.String",
                    "substring",
                    ExternalType::Class("java.lang.String".to_owned()),
                    vec![int(), int()],
                ),
            ],
        )
    }

    /// A world holding `java.lang.Integer` with one member of each half, for
    /// the static/instance split a receiver naming a type turns on.
    fn integer_world() -> World {
        const INTEGER: &str = "java.lang.Integer";
        let int = || ExternalType::Primitive(crate::Primitive::Int);
        let mut parse = method(
            INTEGER,
            "parseInt",
            int(),
            vec![ExternalType::Class("java.lang.String".to_owned())],
        );
        parse.is_static = true;
        let mut max = field(INTEGER, "MAX_VALUE", int());
        max.is_static = true;
        World::default().with(
            INTEGER,
            &[],
            vec![max, parse, method(INTEGER, "intValue", int(), vec![])],
        )
    }

    fn complete(marked: &str) -> Option<Vec<String>> {
        let f = fixture(marked);
        let mut names: Vec<String> =
            member_completions_at(&f.document, &f.tree, &f.files(), &NoExternals, f.offset)?
                .candidates
                .into_iter()
                .map(|candidate| candidate.name)
                .collect();
        names.sort();
        Some(names)
    }

    #[test]
    fn a_dot_on_a_file_local_receiver_offers_its_members() {
        let source = "class Box { int width; String label() { return null; } } \
                      class A { void m() { Box b; b.| } }";
        assert_eq!(
            complete(source),
            Some(vec!["label".to_string(), "width".to_string()])
        );
    }

    #[test]
    fn a_partial_member_name_filters_the_offer() {
        let source = "class Box { int width; int height; } \
                      class A { void m() { Box b; b.wid| } }";
        assert_eq!(complete(source), Some(vec!["width".to_string()]));
    }

    #[test]
    fn a_dot_on_an_unknown_receiver_offers_nothing_rather_than_falling_back() {
        // Some(empty), not None: it IS a member selection, and dumping every
        // local in scope into that popup is the one thing completion must not do.
        let source = "class A { void m() { String s; s.| } }";
        assert_eq!(complete(source), Some(Vec::new()));
    }

    #[test]
    fn a_member_of_an_external_type_is_offered_after_a_dot() {
        // The refusal the whole slice is about, answered: `String` is not a
        // type this file declares, so `s` types as `java.lang.String` and the
        // members come from the world rather than from a file.
        let f = fixture("class A { void m(String s) { s.| } }");
        let offered =
            member_completions_at(&f.document, &f.tree, &f.files(), &string_world(), f.offset)
                .expect("a member selection");
        let rendered: Vec<(String, Option<String>)> = offered
            .candidates
            .into_iter()
            .map(|candidate| (candidate.name, candidate.detail))
            .collect();
        // Detail as well as name: an external member has no buffer to slice a
        // signature out of, so it is rendered from its facts — and a
        // completion list of bare names is what that rendering exists to
        // prevent.
        assert_eq!(
            rendered,
            vec![
                ("length".to_owned(), Some("int length()".to_owned())),
                (
                    "substring".to_owned(),
                    Some("String substring(int, int)".to_owned())
                ),
            ]
        );
        // Not vacuous: the same file with no world behind it answers the empty
        // list Phase 3a answered.
        assert_eq!(
            complete("class A { void m(String s) { s.| } }"),
            Some(Vec::new())
        );
    }

    /// Every list below is `member_completions_at`'s, at the `|`.
    fn offered(marked: &str, externals: &dyn Externals) -> MemberCompletions {
        let f = fixture(marked);
        member_completions_at(&f.document, &f.tree, &f.files(), externals, f.offset)
            .expect("a member selection")
    }

    #[test]
    fn a_member_list_over_a_fully_resolved_receiver_reports_itself_whole() {
        // An external receiver's list is the world's, and this world says the
        // closure it reported was closed.
        let external = offered("class A { void m(String s) { s.| } }", &string_world());
        assert_eq!(names(&external), vec!["length", "substring"]);
        assert!(external.complete);
        // The local half, and reaching it takes a written supertype: a frame
        // that writes one reaches the world through it, so everything above
        // this receiver is accounted for by something.
        let local = offered(
            "class Box extends String { int width; } class A { void m(Box b) { b.| } }",
            &string_world(),
        );
        assert_eq!(names(&local), vec!["width", "length", "substring"]);
        assert!(local.complete);
    }

    #[test]
    fn a_class_that_writes_no_supertype_reports_itself_partial() {
        // Every class extends `java.lang.Object` and almost none writes it
        // down, and this walk follows written names only — so `toString` and
        // its siblings are missing from a list nobody can prove whole. The
        // same file with an `extends` that resolves is the assertion above.
        let found = offered(
            "class Box { int width; } class A { void m(Box b) { b.| } }",
            &string_world(),
        );
        assert_eq!(names(&found), vec!["width"]);
        assert!(!found.complete);
    }

    #[test]
    fn a_supertype_that_did_not_resolve_makes_the_list_report_itself_partial() {
        // `Missing` is a name no file here declares and no world holds, so
        // whatever it declares is absent from this list — and the members that
        // *did* come through are still offered, because a short list is useful
        // and a short list claimed whole is not.
        let found = offered(
            "class Box extends Missing { int width; } class A { void m(Box b) { b.| } }",
            &NoExternals,
        );
        assert_eq!(names(&found), vec!["width"]);
        assert!(!found.complete);
    }

    #[test]
    fn an_incomplete_external_closure_makes_the_list_report_itself_partial() {
        // The world's own flag, carried through rather than re-derived: the
        // same members and the same file as the whole list above, differing
        // only in what the world says about the closure it walked.
        let partial = World::default().with_partial(
            "java.lang.String",
            &[],
            vec![method(
                "java.lang.String",
                "length",
                ExternalType::Primitive(crate::Primitive::Int),
                vec![],
            )],
        );
        let found = offered("class A { void m(String s) { s.| } }", &partial);
        assert_eq!(names(&found), vec!["length"]);
        assert!(!found.complete);
    }

    #[test]
    fn a_cyclic_supertype_chain_reports_itself_partial() {
        // Invalid Java, and ordinary input to a parser that never rejects. A
        // cycle is the one shape where every frame writes an `extends` that
        // resolves — so the unwritten-`java.lang.Object` refusal never fires,
        // and the only guard a cycle hits is the one that is right to cost a
        // diamond nothing. Without a term of its own, this claimed a list
        // missing every member of `Object` was whole.
        let direct = offered(
            "class Box extends Box { int width; } class A { void m(Box b) { b.| } }",
            &string_world(),
        );
        assert_eq!(names(&direct), vec!["width"]);
        assert!(!direct.complete);
        // Two classes, which is the shape a person actually writes by
        // accident, and the guard is hit one frame later.
        let mutual = offered(
            "class Box extends Other { int width; } \
             class Other extends Box { int height; } \
             class A { void m(Box b) { b.| } }",
            &string_world(),
        );
        assert_eq!(names(&mutual), vec!["width", "height"]);
        assert!(!mutual.complete);
    }

    #[test]
    fn a_static_list_on_an_external_type_name_reports_itself_partial() {
        // A nested type is a fact of its own reached by its own name and is
        // deliberately not in an external member list, so `Integer.` is short
        // by whatever `Integer` nests — where the same world's *value*
        // receiver, which cannot select a type through a `.` at all, loses
        // nothing.
        let statics = offered("class A { void m() { Integer.| } }", &integer_world());
        assert_eq!(names(&statics), vec!["MAX_VALUE", "parseInt"]);
        assert!(!statics.complete);
        let value = offered("class A { void m(Integer i) { i.| } }", &integer_world());
        assert_eq!(names(&value), vec!["intValue"]);
        assert!(value.complete);
    }

    fn names(found: &MemberCompletions) -> Vec<&str> {
        found
            .candidates
            .iter()
            .map(|candidate| candidate.name.as_str())
            .collect()
    }

    #[test]
    fn a_dot_on_an_external_type_name_offers_its_static_members_and_not_its_instance_ones() {
        // The reachable path, and the one an editor actually produces.
        // `ScopeTree` binds nothing an import brings in, so `Integer` is bound
        // in neither namespace and `names_a_type` has only the receiver's
        // *type* to go on — without which this would offer `intValue` and
        // never `parseInt`, which is a feature shipped dead.
        let f = fixture("class A { void m() { Integer.| } }");
        let offered =
            member_completions_at(&f.document, &f.tree, &f.files(), &integer_world(), f.offset)
                .expect("a member selection");
        assert_eq!(
            offered
                .candidates
                .iter()
                .map(|candidate| candidate.name.as_str())
                .collect::<Vec<_>>(),
            vec!["MAX_VALUE", "parseInt"]
        );
        // The other half of the same split, through the same reachable path: a
        // receiver holding a value sees the instance member and neither static
        // one.
        let value = fixture("class A { void m(Integer i) { i.| } }");
        let on_a_value = member_completions_at(
            &value.document,
            &value.tree,
            &value.files(),
            &integer_world(),
            value.offset,
        )
        .expect("a member selection");
        assert_eq!(
            on_a_value
                .candidates
                .iter()
                .map(|candidate| candidate.name.as_str())
                .collect::<Vec<_>>(),
            vec!["intValue"]
        );
    }

    #[test]
    fn a_definition_on_an_external_member_answers_nothing_rather_than_a_foreign_span() {
        // A `Definition` carries a span into the *requested* document and a
        // signature sliced out of it, and an external member has neither — so
        // the answer is nothing, through `Member::file()` being `None` and the
        // same guard that refuses a cross-file local member. A jump into the
        // JDK's sources is a non-goal, not an oversight.
        let f = fixture("class A { void m(String s) { s.leng|th } }");
        assert!(
            member_definition_at(&f.document, &f.tree, &f.files(), &string_world(), f.offset)
                .is_none()
        );
        // Not vacuous, and this is the whole point of the test: the member
        // *did* resolve — completion at the same position offers it — so the
        // refusal is the span and not a lookup that failed.
        let offered =
            member_completions_at(&f.document, &f.tree, &f.files(), &string_world(), f.offset)
                .expect("a member selection");
        assert_eq!(
            offered
                .candidates
                .iter()
                .map(|candidate| candidate.name.as_str())
                .collect::<Vec<_>>(),
            vec!["length"]
        );
    }

    #[test]
    fn an_external_member_summary_renders_its_signature_from_facts() {
        // The other half of the same refusal: definition answers nothing
        // because it has no span to answer with, and this is what the same
        // resolution *can* say — a signature built from the facts rather than
        // sliced out of a buffer that never held the declaration.
        let f = fixture("class A { void m(String s) { s.substring|(1, 2); } }");
        let found = external_member_at(&f.document, &f.tree, &f.files(), &string_world(), f.offset)
            .expect("the member the call selects");
        assert_eq!(found.name, "substring");
        assert_eq!(found.kind, DefinitionKind::Method);
        assert_eq!(found.signature, "String substring(int, int)");
        assert_eq!(found.owner, "java.lang.String");
        let text = f.document.text();
        let landed = &text[found.reference.start_byte as usize..found.reference.end_byte as usize];
        assert_eq!(landed, "substring", "the use site, in the requested buffer");
        // Before the `(` the access is a `Value`, which sees no methods at
        // all, and the `MethodRef` retry is what keeps hover from blinking out
        // mid-keystroke — on an external member exactly as on a declared one,
        // because both come through `selected_member`.
        let typing = fixture("class A { void m(String s) { s.leng|th } }");
        assert_eq!(
            external_member_at(
                &typing.document,
                &typing.tree,
                &typing.files(),
                &string_world(),
                typing.offset,
            )
            .expect("the method, via the MethodRef retry")
            .signature,
            "int length()"
        );
    }

    #[test]
    fn an_external_members_summary_carries_the_site_the_world_gave_it() {
        // `external_member_at` copies `ExternalMember::site` through unchanged
        // — it is `Externals::members`'s answer to keep, not this query's to
        // build, so the value asserted here is exactly what the world was
        // handed below.
        let int = || ExternalType::Primitive(crate::Primitive::Int);
        let site = ExternalSite {
            doc_span: None,
            source: "java.base/java/lang/String.java".to_owned(),
            name_start: 100,
            name_end: 106,
            name_range: None,
        };
        let mut length = method("java.lang.String", "length", int(), vec![]);
        length.site = Some(site.clone());
        let world = World::default().with("java.lang.String", &[], vec![length]);
        let f = fixture("class A { void m(String s) { s.leng|th } }");
        let found = external_member_at(&f.document, &f.tree, &f.files(), &world, f.offset)
            .expect("the method the world holds");
        assert_eq!(found.site, Some(site));
    }

    /// `@Column(name = …)`'s `name` is `Column.name()` — never the field
    /// `name` it annotates — whether the file or the world declares `Column`.
    #[test]
    fn an_annotation_elements_key_is_the_annotation_types_element() {
        let local = member_definition(
            "@interface Column { String name(); int name = 1; } \
             class A { @Column(na|me = \"n\") String name; }",
        )
        .expect("the element the file declares");
        assert_eq!(local.name, "name");
        assert_eq!(
            local.kind,
            DefinitionKind::from(javelin_parser::DeclKind::AnnotationElement)
        );
        let mut element = method(
            "a.Mapping",
            "value",
            ExternalType::Class("java.lang.String".into()),
            vec![],
        );
        element.kind = javelin_parser::DeclKind::AnnotationElement;
        let world = World::default()
            .with("java.lang.String", &[], Vec::new())
            .with("a.Mapping", &[], vec![element]);
        let f = fixture(
            "import a.Mapping; class A { String value; @a.Mapping(val|ue = \"/\") void m() {} }",
        );
        let found = external_member_at(&f.document, &f.tree, &f.files(), &world, f.offset)
            .expect("the element the world holds");
        assert_eq!(
            (found.name.as_str(), found.owner.as_str()),
            ("value", "a.Mapping")
        );
        // `a` may be a member type the tiers do not see, which `a.Mapping`
        // would then name instead.
        assert!(
            member_definition_at(&f.document, &f.tree, &f.files(), &world, f.offset).is_none(),
            "nor the field it shares a name with"
        );
        let shadowed = world.shadowing("a");
        assert!(
            external_member_at(&f.document, &f.tree, &f.files(), &shadowed, f.offset).is_none(),
            "nor an element of a type the annotation may not mean"
        );
    }

    #[test]
    fn a_case_label_under_an_enum_selector_is_the_enums_constant() {
        let local = member_definition(
            "enum Day { MON, TUE } class A { static final int MON = 1; \
             int f(Day d) { switch (d) { case M|ON: return 1; } return 0; } }",
        )
        .expect("the constant, not the field the scope binds");
        assert_eq!(
            local.kind,
            DefinitionKind::from(javelin_parser::DeclKind::EnumConstant)
        );
        let mut mon = field("a.Day", "MON", ExternalType::Class("a.Day".into()));
        mon.kind = javelin_parser::DeclKind::EnumConstant;
        mon.is_static = true;
        let world = World::default().with("a.Day", &[], vec![mon]);
        let f = fixture(
            "import a.Day; class A { int f(Day d) { \
             return switch (d) { case TUE, M|ON -> 1; default -> 0; }; } }",
        );
        let found = external_member_at(&f.document, &f.tree, &f.files(), &world, f.offset)
            .expect("the constant the world holds");
        assert_eq!(
            (found.name.as_str(), found.owner.as_str()),
            ("MON", "a.Day")
        );
        for (source, why) in [
            (
                "class A { static final int MAX = 1; void f(int i) { switch (i) { case M|AX: } } }",
                "an int selector's constant is the scope's",
            ),
            (
                "class Box { static final int MAX = 1; } \
                 class A { void f(Box b) { switch (b) { case M|AX: } } }",
                "a field of the name that is no enum constant",
            ),
            (
                "class A { void f(Unknown u) { switch (u) { case M|ON: } } }",
                "a selector nothing types",
            ),
            (
                "enum Kind { ONE } class A { int kind; static final int ONE = 1; \
                 void f(Object o) { if (o instanceof Kind kind) { } \
                 switch (kind) { case O|NE: break; } } }",
                "a pattern variable past its `if` may be the int field",
            ),
        ] {
            assert!(member_definition(source).is_none(), "{why}");
        }
        let nested = member_definition(
            "enum Day { MON } enum Kind { MON } class A { void f(Day d, Kind k) { \
             switch (d) { case MON: switch (k) { case M|ON -> {} } } } }",
        )
        .expect("the inner switch's constant");
        assert_eq!(nested.signature, "MON");
        assert!(nested.span.start_byte > 20, "Kind's MON, not Day's");
    }

    #[test]
    fn the_scope_answers_a_case_label_only_under_a_primitive_a_box_or_a_string() {
        let scoped = |marked: &str, externals: &dyn Externals| {
            let f = fixture(marked);
            scope_answers_case_label(&f.document, &f.tree, &f.files(), externals, f.offset)
        };
        let world = World::default()
            .with("java.lang.String", &[], Vec::new())
            .with("java.lang.Integer", &[], Vec::new());
        for (source, expected, why) in [
            (
                "class A { int M = 1; int f() { return M|; } }",
                true,
                "not a label",
            ),
            (
                "class A { void f(int i) { switch (i) { case M|: } } }",
                true,
                "an int",
            ),
            (
                "class A { void f(String s) { switch (s) { case M|: } } }",
                true,
                "a String",
            ),
            (
                "class A { void f(Integer i) { switch (i) { case M|: } } }",
                true,
                "a box",
            ),
            (
                "class A { int k; void f(Object o) { if (o instanceof Integer k) { } \
                 switch (k) { case M|: } } }",
                false,
                "a pattern variable that may be the field",
            ),
            (
                "enum Day { MON } class A { void f(Day d) { switch (d) { case M|ON: } } }",
                false,
                "an enum",
            ),
            (
                "class A { void f(Day d) { switch (d) { case M|ON: } } }",
                false,
                "a type nothing settles, which may be an enum",
            ),
        ] {
            assert_eq!(scoped(source, &world), expected, "{why}");
        }
    }

    #[test]
    fn an_external_member_from_a_world_with_no_site_summarises_without_one() {
        // `string_world()`'s members come from the `method()` fixture helper,
        // which leaves `site` at `None` — a hand-built world cannot say where
        // a member is written, and the summary must say so honestly rather
        // than inventing a location.
        let f = fixture("class A { void m(String s) { s.leng|th } }");
        let found = external_member_at(&f.document, &f.tree, &f.files(), &string_world(), f.offset)
            .expect("the method the world holds");
        assert_eq!(found.site, None);
    }

    #[test]
    fn an_unqualified_call_to_an_inherited_external_method_is_summarised() {
        // `greet()` with no receiver, inside a class whose supertype no
        // reachable file declares: the enclosing type's closure reaches the
        // world's member, and the summary is the one a `this.greet()` gets.
        let world = World::default().with(
            "p.Base",
            &[],
            vec![method("p.Base", "greet", ExternalType::Void, vec![])],
        );
        let f = fixture("class A extends Base { void m() { gre|et(); } }");
        let found = external_member_at(&f.document, &f.tree, &f.files(), &world, f.offset)
            .expect("the inherited method");
        assert_eq!(found.signature, "void greet()");
        assert_eq!(found.owner, "p.Base");
        let text = f.document.text();
        assert_eq!(
            &text[found.reference.start_byte as usize..found.reference.end_byte as usize],
            "greet"
        );
    }

    #[test]
    fn a_local_method_called_unqualified_is_not_summarised_as_external() {
        let world = World::default().with(
            "p.Base",
            &[],
            vec![method("p.Base", "greet", ExternalType::Void, vec![])],
        );
        let source = "class A extends Base { void greet() {} void m() { gre|et(); } }";
        let f = fixture(source);
        assert!(external_member_at(&f.document, &f.tree, &f.files(), &world, f.offset).is_none());
        // Not vacuous: the call resolves, to the own method, through the
        // query that owns a local declaration.
        assert_eq!(
            call_definition(source).expect("the own method").name,
            "greet"
        );
    }

    #[test]
    fn a_local_member_is_not_answered_by_the_external_query() {
        // The two `.` queries partition rather than overlap: a member a
        // reachable file declares is `member_definition_at`'s answer, so a
        // caller trying both in either order gets one answer and never two.
        let source = "class Box { int width; } class A { void m() { Box b; b.wid|th } }";
        let f = fixture(source);
        assert!(
            external_member_at(&f.document, &f.tree, &f.files(), &string_world(), f.offset)
                .is_none()
        );
        // Not vacuous: the same position does resolve, through the query that
        // owns it.
        assert_eq!(member_definition(source).expect("the field").name, "width");
    }

    #[test]
    fn an_ambiguous_external_member_answers_nothing() {
        // `sole_candidate` is the one filter, and rendering from facts does
        // not get to skip it: `pick()` and `pick(int)` both survive a
        // `MethodRef` access, which carries no arity to tell them apart, so
        // the answer is nothing rather than whichever the world listed first.
        let int = || ExternalType::Primitive(crate::Primitive::Int);
        let overloaded = World::default().with(
            "java.lang.String",
            &[],
            vec![
                method("java.lang.String", "pick", int(), vec![]),
                method("java.lang.String", "pick", int(), vec![int()]),
            ],
        );
        let f = fixture("class A { void m(String s) { s.pi|ck } }");
        assert!(
            external_member_at(&f.document, &f.tree, &f.files(), &overloaded, f.offset).is_none()
        );
        // Not vacuous: drop one overload and the identical position renders.
        let sole = World::default().with(
            "java.lang.String",
            &[],
            vec![method("java.lang.String", "pick", int(), vec![])],
        );
        assert_eq!(
            external_member_at(&f.document, &f.tree, &f.files(), &sole, f.offset)
                .expect("the only method of that name")
                .signature,
            "int pick()"
        );
    }

    #[test]
    fn a_cursor_that_is_not_selecting_a_member_declines_the_question() {
        let source = "class A { void m() { int x = 1; x| } }";
        assert_eq!(complete(source), None);
    }

    #[test]
    fn a_static_receiver_offers_static_members_only() {
        let source = "class Box { static int COUNT; int width; } class A { void m() { Box.| } }";
        assert_eq!(complete(source), Some(vec!["COUNT".to_string()]));
    }

    /// A bare name no scope binds is the field the innermost enclosing type
    /// inherits of that name (JLS 6.4.1, 15.11), and nothing where Java's
    /// answer is not one such field.
    #[test]
    fn a_bare_inherited_field_lands_on_the_supertypes_declaration() {
        let jump = |source: &str| {
            let f = fixture(source);
            let found =
                member_definition_at(&f.document, &f.tree, &f.files(), &NoExternals, f.offset)?;
            let text = &f.document.text()[found.span.start_byte as usize..];
            Some(format!(
                "{}: {}",
                found.name,
                text.split(';').next().unwrap_or("")
            ))
        };
        for (source, expected, why) in [
            (
                "class Base { int count; } class Sub extends Base { int m() { return cou|nt; } }",
                Some("count: int count"),
                "a field the supertype declares",
            ),
            (
                "class Root { long count; } class Base extends Root { } \
                 class Sub extends Base { long m() { return cou|nt; } }",
                Some("count: long count"),
                "two supertypes up",
            ),
            (
                "class Base { int count; } class Outer extends Base { \
                 class Inner { int m() { return cou|nt; } } }",
                Some("count: int count"),
                "an enclosing type's inherited field, past a nested type without one",
            ),
            (
                "interface Limits { int MAX = 1; } class Sub implements Limits { int m() { return MA|X; } }",
                Some("MAX: int MAX = 1"),
                "an interface's constant",
            ),
            (
                "class Base { private int count; } class Sub extends Base { int m() { return cou|nt; } }",
                None,
                "a private field is not inherited",
            ),
            (
                "class Base { int count; } class Sub extends Base { int m(int count) { return cou|nt; } }",
                None,
                "a parameter of the name is the scope tree's, not a member's",
            ),
            (
                "class Base { int count; } class Sub extends Base implements Missing { \
                 int m() { return cou|nt; } }",
                Some("count: int count"),
                "a short list that holds the field still holds it",
            ),
            (
                "class Sub extends Missing { int m() { return cou|nt; } }",
                None,
                "a short list without it",
            ),
            (
                "class Base { int count; } class Other { int count; } \
                 class Outer extends Other { class Inner extends Base { int m() { return cou|nt; } } }",
                None,
                "a type writing a supertype inside one that writes one",
            ),
            (
                "class Base { int count; } class Sub extends Base { void m(int k) { \
                 switch (k) { case 1: int count = 0; break; case 2: cou|nt = 5; } } }",
                None,
                "a local an earlier case group declares",
            ),
            (
                "class Base { static final int FOO = 1; } @Ann(FO|O) class Sub extends Base { }",
                None,
                "a type's own annotation is outside its body",
            ),
        ] {
            assert_eq!(jump(source).as_deref(), expected, "{why}");
        }
    }

    #[test]
    fn definition_lands_on_the_members_own_name() {
        let source = "class Box { int width; } class A { void m() { Box b; b.wid|th } }";
        let f = fixture(source);
        let found = member_definition_at(&f.document, &f.tree, &f.files(), &NoExternals, f.offset)
            .expect("the field");
        assert_eq!(found.name, "width");
        assert_eq!(found.kind, javelin_parser::DefinitionKind::Field);
        let text = f.document.text();
        let landed = &text[found.name_span.start_byte as usize..found.name_span.end_byte as usize];
        assert_eq!(landed, "width");
    }

    #[test]
    fn a_member_definitions_file_is_the_file_that_declares_it() {
        let source = "class Box { int width; } class A { void m() { Box b; b.wid|th } }";
        let f = fixture(source);
        let found = member_definition_at(&f.document, &f.tree, &f.files(), &NoExternals, f.offset)
            .expect("the field");
        assert_eq!(found.file, FileId::new(FILE));
    }

    #[test]
    fn definition_on_an_unknown_receiver_finds_nothing() {
        let source = "class A { void m() { String s; s.leng|th } }";
        let f = fixture(source);
        assert!(
            member_definition_at(&f.document, &f.tree, &f.files(), &NoExternals, f.offset)
                .is_none()
        );
    }

    /// Go to the declaration of the member selected at the offset marked `|`.
    fn member_definition(marked: &str) -> Option<Definition> {
        let f = fixture(marked);
        member_definition_at(&f.document, &f.tree, &f.files(), &NoExternals, f.offset)
    }

    #[test]
    fn a_fixed_arity_overload_the_arguments_rule_out_does_not_hide_the_variadic_one() {
        // slf4j's shape: `info(Marker, String, Object, Object)` beside
        // `info(String, Object...)`, called with a format and three values.
        let found = member_definition(
            "class Marker {} class Msg {} class A { \
             void f(Marker m, Msg a, Msg b) {} void f(Msg a, Msg... rest) {} \
             void g(A o) { o.|f(new Msg(), new Msg(), new Msg()); } }",
        )
        .expect("the variadic one");
        assert!(
            found.signature.contains("Msg... rest"),
            "{}",
            found.signature
        );
    }

    #[test]
    fn a_qualified_call_with_two_same_arity_overloads_answers_nothing_rather_than_the_first() {
        // This is the wrong jump that shipped before Task 2: `member_definition_at`
        // took the first-declared `g`, so `o.g(1)` always landed on `g(int)`
        // regardless of which overload the call meant. `g(String)`'s parameter
        // is `java.lang.String`, which this phase cannot resolve, so the
        // argument-type narrowing has nothing to rule it out with — this stays
        // ambiguous, and Phase 3's classpath is what changes that.
        let source = "class O { int g(int i) { return i; } int g(String s) { return 0; } } \
                      class A { O o; void m() { o.g|(1); } }";
        assert!(member_definition(source).is_none());
    }

    #[test]
    fn a_qualified_call_lands_on_the_method_even_when_a_same_named_field_is_declared_first() {
        // Namespace, not declaration order: a field `g` and a method `g(int)`
        // occupy different Java namespaces, so `o.g(1)` can only mean the call
        // — regardless of which one the type declares first.
        let source = "class O { int g; int g(int i) { return i; } } \
                      class A { O o; void m() { o.g|(1); } }";
        let found = member_definition(source).expect("the method");
        assert_eq!(found.kind, javelin_parser::DefinitionKind::Method);
    }

    #[test]
    fn a_comment_inside_the_argument_list_does_not_hide_the_qualified_call() {
        // Regression: `project_arguments` used to count `line_comment` and
        // `block_comment` as arguments, so `o.f(1 /*c*/)` looked like a
        // two-argument call, `f(int)` failed the arity filter, and definition
        // landed on nothing where a plain `o.f(1)` still landed on `f(int)`.
        let source = "class O { int f(int i) { return i; } } \
                      class A { O o; void m() { o.f|(1 /*c*/); } }";
        let found = member_definition(source).expect("f(int), same as without the comment");
        assert_eq!(found.name, "f");
        assert_eq!(found.kind, javelin_parser::DefinitionKind::Method);
    }

    #[test]
    fn a_value_access_still_finds_a_field_that_is_the_only_member_of_that_name() {
        let source = "class O { int g; } class A { O o; void m() { o.g| ; } }";
        let found = member_definition(source).expect("the field");
        assert_eq!(found.kind, javelin_parser::DefinitionKind::Field);
    }

    #[test]
    fn definition_before_the_call_parenthesis_still_lands_on_the_method() {
        // `o.lab|el` with no `(` typed yet is `Access::Value`, which sees no
        // methods on its own. The 2.2 fallback retries as `MethodRef` so
        // definition and hover do not blink out mid-keystroke, before the `(`
        // that would make this an ordinary `Access::Call`.
        let source = "class O { String label() { return null; } } \
                      class A { O o; void m() { o.lab|el ; } }";
        let found = member_definition(source).expect("the method, via the MethodRef fallback");
        assert_eq!(found.kind, javelin_parser::DefinitionKind::Method);
    }

    #[test]
    fn definition_before_the_call_parenthesis_on_two_overloads_answers_nothing() {
        // The fallback widens which *kind* is visible, never which candidate
        // wins: `MethodRef` carries no arity to filter on, so both overloads of
        // `g` survive and `sole_candidate` refuses rather than taking the first.
        let source = "class O { int g(int i) { return i; } int g(String s) { return 0; } } \
                      class A { O o; void m() { o.g| ; } }";
        assert!(member_definition(source).is_none());
    }

    #[test]
    fn a_fixed_arity_overload_beats_a_variadic_one_at_the_same_argument_count() {
        // JLS 15.12.2: fixed-arity applicability is tried before variable-arity
        // applicability, so `f(1, 2)` compiles to `f(int, int)` even though
        // `f(int, int...)` also fits by count.
        let source = "class O { int f(int a, int b) { return a; } \
                      int f(int a, int... xs) { return a; } } \
                      class A { O o; void m() { o.f|(1, 2); } }";
        let found = member_definition(source).expect("the fixed-arity overload");
        assert_eq!(found.signature, "int f(int a, int b)");
    }

    #[test]
    fn a_receiver_with_thousands_of_members_answers_without_walking_the_file_per_member() {
        // The breadth counterpart to `members::an_exploding_generic_chain_...`,
        // which bounds depth. Every member's declared type is a class this file
        // declares, so resolving one runs `FileSyntax::declaration_at` over the
        // whole file — doing that per member is O(members x file) on the request
        // loop, which is what `members::member_type` exists to keep off it.
        //
        // The bound is wall-clock and deliberately loose: measured in a debug
        // build, lazy resolution answers in 0.7 ms and eager resolution in
        // 380 ms, so 100 ms sits 135x above one and 3.8x below the other. It
        // is here to fail on a re-eagered resolution, not to police
        // milliseconds on a loaded CI box.
        let members = 8_000;
        let mut source = String::from("class Elem {}\nclass Big {\n");
        for i in 0..members {
            source.push_str(&format!("  Elem m{i}() {{ return null; }}\n"));
        }
        source.push_str("}\nclass A { void go(Big b) { b.| ; } }\n");

        let f = fixture(&source);
        let started = std::time::Instant::now();
        let offered =
            member_completions_at(&f.document, &f.tree, &f.files(), &NoExternals, f.offset)
                .expect("a member selection");
        let elapsed = started.elapsed();
        assert_eq!(offered.candidates.len(), MAX_CANDIDATES);
        assert!(
            elapsed < std::time::Duration::from_millis(100),
            "{members} members took {elapsed:?}"
        );
    }

    /// Go to the declaration of the unqualified call marked with `|`.
    fn call_definition(marked: &str) -> Option<Definition> {
        let f = fixture(marked);
        call_definition_at(&f.document, &f.tree, &f.files(), &NoExternals, f.offset)
    }

    #[test]
    fn definition_on_an_unqualified_call_lands_on_the_method() {
        let source = "class A { int count() { return 1; } void m() { co|unt(); } }";
        let found = call_definition(source).expect("the method");
        assert_eq!(found.name, "count");
        assert_eq!(found.kind, javelin_parser::DefinitionKind::Method);
        assert_eq!(found.signature, "int count()");
    }

    #[test]
    fn definition_on_an_unqualified_call_with_two_candidates_of_the_same_arity_answers_nothing() {
        // Both take one argument, so arity cannot separate them — and neither
        // can the argument, because `g(String)`'s parameter is the JDK's
        // `String` and an unresolvable name could be `Object`, `Number` or
        // `Comparable`, each of which takes an `int`. That is Phase 3's
        // classpath, not a gap here. Nothing, not the first.
        let source = "class A { int g(int i) { return i; } int g(String s) { return 0; } \
                      void m() { g|(1); } }";
        assert!(call_definition(source).is_none());
    }

    #[test]
    fn definition_on_an_unqualified_call_picks_the_overload_that_fits_the_arity() {
        // `g()` against `g()`/`g(int)` is unambiguous by count alone: this is
        // the arity filter doing its job, not a pick between rival candidates.
        let source = "class A { int g() { return 1; } int g(int i) { return i; } \
                      void m() { g|(); } }";
        let found = call_definition(source).expect("the no-argument overload");
        assert_eq!(found.signature, "int g()");
    }

    #[test]
    fn definition_on_an_unqualified_call_to_a_name_the_file_does_not_declare_answers_nothing() {
        // Inherited from a supertype this file cannot see, or static-imported:
        // either way the open buffer has no declaration, and Phase 3 is where
        // that changes.
        let source = "class A { void m() { print|ln(); } }";
        assert!(call_definition(source).is_none());
        // Not vacuous: the cursor really is on an unqualified call, so the
        // `None` above is `sole_candidate` finding no declaration rather than
        // `unqualified_call_at` having quietly stopped recognising the shape.
        let f = fixture(source);
        assert!(unqualified_call_at(&f.document, &f.tree, f.offset).is_some());
    }

    #[test]
    fn definition_on_an_unqualified_varargs_call_still_needs_its_fixed_parameters() {
        // The floor is `arity + 1 >= declared`: `join(String sep, int... xs)`
        // still needs its `sep`, so a zero-argument call fits nothing — even
        // though it is the only method of that name in the file, which is
        // exactly the pick this must not make.
        let source = "class A { String join(String sep, int... xs) { return sep; } \
                      void m() { jo|in(); } }";
        assert!(call_definition(source).is_none());
        // Not vacuous: the cursor is on an unqualified call, and one argument
        // — the fixed parameter, with an empty variadic tail — does land.
        let f = fixture(source);
        assert!(unqualified_call_at(&f.document, &f.tree, f.offset).is_some());
        let fixed_only = "class A { String join(String sep, int... xs) { return sep; } \
                          void m() { jo|in(\"a\"); } }";
        assert!(call_definition(fixed_only).is_some());
    }

    #[test]
    fn definition_on_an_unqualified_varargs_call_accepts_extra_arguments() {
        // The other side of the same floor: everything past the fixed
        // parameters is the variadic tail, so two trailing arguments fit a
        // method declaring one of them.
        let source = "class A { String join(String sep, int... xs) { return sep; } \
                      void m() { jo|in(\"a\", 1, 2); } }";
        let found = call_definition(source).expect("the varargs method");
        assert_eq!(found.signature, "String join(String sep, int... xs)");
    }

    #[test]
    fn an_unqualified_call_shadowed_by_an_own_method_lands_on_the_own_one() {
        // Two candidates fit by name and arity, and this is still not the
        // ambiguous case: an override hides what it overrides, so the call can
        // only mean `A`'s. The doc comments tell them apart — the signatures
        // are identical, which is the point of an override.
        let source = "class Base { /** The inherited one. */ int id() { return 0; } } \
                      class A extends Base { /** The own one. */ int id() { return 1; } \
                      void m() { i|d(); } }";
        let found = call_definition(source).expect("the overriding method");
        assert_eq!(found.signature, "int id()");
        assert!(
            found
                .doc
                .as_deref()
                .is_some_and(|doc| doc.contains("The own one")),
            "{:?}",
            found.doc
        );
    }

    #[test]
    fn a_call_inside_an_enum_constants_body_answers_nothing() {
        // The body is an anonymous class: Java picks its `log(String)`, which
        // no list the enum walks holds.
        let source = "enum Op { A { void log(String s) {} void m() { lo|g(\"x\"); } }; \
                      void log(Object o) {} }";
        assert_eq!(call_definition(source).map(|found| found.signature), None);
    }

    #[test]
    fn an_unqualified_call_inside_a_nested_class_reaches_the_outer_classs_method() {
        // JLS 15.12.1: the innermost enclosing type of which a method of that
        // name is a member, whatever its arity.
        let reached = |source: &str| call_definition(source).map(|found| found.signature);
        assert_eq!(
            reached(
                "class Outer { int outer() { return 1; } \
                 class Inner { int inner() { return 2; } void m() { out|er(); } } }"
            )
            .as_deref(),
            Some("int outer()")
        );
        assert_eq!(
            reached(
                "class Outer { int outer() { return 1; } class Middle { \
                 class Inner { int outer; void m() { out|er(); } } } }"
            )
            .as_deref(),
            Some("int outer()"),
            "two levels out, past a field of the name, which is no method"
        );
        assert_eq!(
            reached(
                "class Outer { int inner() { return 1; } \
                 class Inner { int inner() { return 2; } void m() { inn|er(); } } }"
            )
            .as_deref(),
            Some("int inner()"),
            "the nested class's own method"
        );
        assert_eq!(
            reached(
                "class Base { private int f() { return 1; } } class Outer { void f() {} \
                 class Inner extends Base { void m() { f|(); } } }"
            )
            .as_deref(),
            Some("void f()"),
            "a supertype's `private` method is no member of `Inner`"
        );
        assert_eq!(
            reached(
                "class Outer { void outer() {} \
                 class Inner { @SuppressWarnings(\"x\") int a; void m() { out|er(); } } }"
            )
            .as_deref(),
            Some("void outer()"),
            "`java.lang`'s own annotations add nothing"
        );
        for (source, why) in [
            (
                "class Outer { int outer() { return 1; } \
                 class Inner { int outer(int i) { return i; } void m() { out|er(); } } }",
                "a method of the name in `Inner` ends the search there, and javac \
                 rejects the call",
            ),
            (
                "class Outer { int outer() { return 1; } \
                 class Inner extends Nowhere { void m() { out|er(); } } }",
                "a supertype nothing resolves may declare it",
            ),
            (
                "class Outer { int outer() { return 1; } \
                 enum E { A; void m() { out|er(); } } }",
                "an enum's members are not all written",
            ),
            (
                "class Outer { int outer() { return 1; } \
                 record R() { void m() { out|er(); } } }",
                "nor a record's",
            ),
            (
                "class Outer { int annotationType() { return 1; } \
                 @interface Ann { class Inner { void m() { annotation|Type(); } } } }",
                "an annotation type has `Annotation`'s methods unwritten",
            ),
            (
                "class Outer { public String toString() { return null; } \
                 class Inner { void m() { toStr|ing(); } } }",
                "every class has `Object`'s methods, so `Inner` is searched",
            ),
            (
                "class Err {} class Up {} class Outer extends Up { void outer() {} \
                 class Inner extends Err { void m() { out|er(); } } }",
                "`Up` may declare a member type `Err`, which `Inner` would extend",
            ),
            (
                "class Outer { void outer() {} \
                 class Inner { @Delegate Object o; void m() { out|er(); } } }",
                "a generator may add the method to `Inner`",
            ),
            (
                "class Outer { void outer() {} @Data class Inner { int x; void m() { out|er(); } } }",
                "nor to an annotated `Inner`",
            ),
            (
                "@Getter class Base { int x; } class Outer { void outer() {} \
                 class Inner extends Base { void m() { out|er(); } } }",
                "nor to a supertype that is",
            ),
        ] {
            assert!(reached(source).is_none(), "{why}");
        }
    }

    #[test]
    fn an_unqualified_call_inside_an_anonymous_class_does_not_reach_the_outer_classs_method() {
        // The other half of the ceiling its named-nested sibling above pins:
        // an anonymous class is a type declaration too, so `enclosing_type`
        // answers with it and the call resolves against it alone. Neither of
        // the two classes a Java programmer would expect to reach is reached.
        // Not `A`, which lexically encloses the `new`, because the search for
        // the class to search cannot see past a class whose supertype it does
        // not know. And not `Base`, which the anonymous class extends,
        // for a second one: `FileSyntax` records the instantiated type as the
        // anonymous class's *name* — the closest thing to a name it has — and
        // never as its `extends`, and `members_of` follows supertypes, not
        // names.
        let source = "class Base { int base() { return 1; } } \
                      class A { int outer() { return 2; } void m() { \
                      Base b = new Base() { int own() { return 3; } \
                      void go() { out|er(); } }; } }";
        assert!(call_definition(source).is_none());
        // Not vacuous, twice over, exactly as the named-nested case is: the
        // cursor is on an unqualified call, and the identical shape aimed at a
        // method the anonymous body itself declares resolves.
        let f = fixture(source);
        assert!(unqualified_call_at(&f.document, &f.tree, f.offset).is_some());
        let inherited = source.replace("out|er()", "ba|se()");
        assert!(call_definition(&inherited).is_none());
        let own = source.replace("out|er()", "o|wn()");
        assert_eq!(
            call_definition(&own)
                .expect("the anonymous class's own method")
                .signature,
            "int own()"
        );
    }

    #[test]
    fn definition_on_an_inherited_unqualified_call_lands_on_the_supertypes_method() {
        let source = "class Base { int id() { return 0; } } \
                      class A extends Base { void m() { i|d(); } }";
        let found = call_definition(source).expect("the inherited method");
        assert_eq!(found.name, "id");
        assert_eq!(found.signature, "int id()");
    }

    #[test]
    fn a_qualified_call_is_not_answered_here() {
        // `member_definition_at`'s question, and answering it twice from two
        // rules is how the two answers start to disagree.
        let source = "class O { int g() { return 1; } } \
                      class A { O o; void m() { o.g|(); } }";
        assert!(call_definition(source).is_none());
        let f = fixture(source);
        assert!(
            member_definition_at(&f.document, &f.tree, &f.files(), &NoExternals, f.offset)
                .is_some()
        );
    }

    #[test]
    fn an_inherited_member_is_offered_and_resolvable() {
        let source = "class Base { int id; } class Box extends Base {} \
                      class A { void m() { Box b; b.| } }";
        assert_eq!(complete(source), Some(vec!["id".to_string()]));
    }

    // Elimination by argument type. Each test below is one decision, and the
    // decisions split two ways: a pick where the open file demonstrates the
    // narrowing, and nothing everywhere else. The refusals are not gaps to be
    // papered over — most of them are the Phase 3 classpath, and their comments
    // say which.

    #[test]
    fn definition_on_a_call_with_a_file_local_argument_picks_the_overload_that_takes_it() {
        // The separable case, and the one a real domain model has: both
        // parameter types are declared in this file, so `new Foo()` rules
        // `f(Bar)` out with no classpath and no guess.
        let source = "class Foo {} class Bar {} \
                      class A { int f(Foo p) { return 1; } int f(Bar p) { return 2; } \
                      void m() { f|(new Foo()); } }";
        let found = call_definition(source).expect("the Foo overload");
        assert_eq!(found.signature, "int f(Foo p)");
        // The other way round, so the pick is the argument and not the order
        // the overloads are declared in.
        let swapped = source.replace("new Foo()", "new Bar()");
        assert_eq!(
            call_definition(&swapped)
                .expect("the Bar overload")
                .signature,
            "int f(Bar p)"
        );
    }

    #[test]
    fn definition_on_a_call_with_an_int_argument_picks_the_primitive_overload() {
        // Boxing an `int` reaches only the JDK's supertypes, and `Foo` is not
        // one of them, so the primitive overload is the only one left.
        let source = "class Foo {} \
                      class A { int f(Foo p) { return 1; } int f(int i) { return i; } \
                      void m() { f|(1); } }";
        assert_eq!(
            call_definition(source).expect("the int overload").signature,
            "int f(int i)"
        );
    }

    #[test]
    fn definition_on_a_call_with_a_null_argument_picks_the_reference_overload() {
        // `null` is passable to every reference type and to no primitive, so
        // it separates these two the other way round from `f(1)` above.
        let source = "class Foo {} \
                      class A { int f(Foo p) { return 1; } int f(int i) { return i; } \
                      void m() { f|(null); } }";
        assert_eq!(
            call_definition(source).expect("the Foo overload").signature,
            "int f(Foo p)"
        );
    }

    #[test]
    fn definition_on_a_null_argument_between_two_reference_overloads_answers_nothing() {
        // `null` fits both, which is genuine ambiguity rather than a ceiling:
        // `f(null)` between two reference overloads does not compile in Java
        // either without a cast.
        let source = "class Foo {} class Bar {} \
                      class A { int f(Foo p) { return 1; } int f(Bar p) { return 2; } \
                      void m() { f|(null); } }";
        assert!(call_definition(source).is_none());
    }

    #[test]
    fn definition_on_a_call_whose_argument_is_a_poly_expression_answers_nothing() {
        // A lambda's and a method reference's type is the target type, which is
        // the very thing being chosen — so neither can exclude anything, and
        // both project as `Expr::Opaque` precisely because there is nothing to
        // project. Refusing is the answer; poly-expression typing needs the
        // functional interface, which is the JDK's.
        let lambda = "class Foo {} class Bar {} \
                      class A { int f(Foo p) { return 1; } int f(Bar p) { return 2; } \
                      void m() { f|(() -> {}); } }";
        assert!(call_definition(lambda).is_none());
        let method_ref = "class Foo {} class Bar {} \
                          class A { int f(Foo p) { return 1; } int f(Bar p) { return 2; } \
                          void h() {} void m() { f|(this::h); } }";
        assert!(call_definition(method_ref).is_none());
        // Not vacuous: the cursor really is on an unqualified call in both, and
        // the identical shape with an argument the file can type does resolve.
        for source in [lambda, method_ref] {
            let f = fixture(source);
            assert!(unqualified_call_at(&f.document, &f.tree, f.offset).is_some());
        }
        let typed = lambda.replace("() -> {}", "new Foo()");
        assert!(call_definition(&typed).is_some());
    }

    #[test]
    fn definition_on_two_overloads_differing_only_in_a_jdk_parameter_type_answers_nothing() {
        // Both parameter types are the JDK's, so neither can be ruled out: an
        // unresolvable name could be `Object`, `Number` or `Comparable`, each
        // of which takes an `int`. This is the Phase 3 classpath, not a bug and
        // not something argument types can close.
        let source = "class A { int f(String s) { return 0; } int f(Integer i) { return 1; } \
                      void m() { f|(1); } }";
        assert!(call_definition(source).is_none());
    }

    #[test]
    fn definition_on_a_call_whose_argument_has_an_unresolvable_type_answers_nothing() {
        // The argument side of the same ceiling: `u`'s type is another file's,
        // so it could be a subtype of either parameter type. Phase 3 again.
        let source = "class Foo {} class Bar {} \
                      class A { int f(Foo p) { return 1; } int f(Bar p) { return 2; } \
                      void m(Unresolvable u) { f|(u); } }";
        assert!(call_definition(source).is_none());
        // Not vacuous: give the same parameter a file-local type and the same
        // call resolves, so the `None` above is the unresolvable argument and
        // not the shape.
        let resolvable = source.replace("Unresolvable u", "Foo u");
        assert_eq!(
            call_definition(&resolvable)
                .expect("the Foo overload")
                .signature,
            "int f(Foo p)"
        );
    }

    #[test]
    fn definition_on_an_int_argument_between_an_int_and_a_long_overload_is_the_int_one() {
        // `int` widens to `long` (JLS 5.1.2), so both overloads survive
        // elimination, and `f(int)` is the most specific (JLS 15.12.2.5).
        let source = "class A { int f(int i) { return i; } int f(long l) { return 0; } \
                      void m() { f|(1); } }";
        assert_eq!(
            call_definition(source).expect("f(int)").signature,
            "int f(int i)"
        );
    }

    #[test]
    fn definition_between_a_supertype_and_a_subtype_overload_is_the_subtype_one() {
        // `new Dog()` is applicable to both `f(Animal)` and `f(Dog)`, and
        // `f(Dog)` is the most specific.
        let source = "class Animal {} class Dog extends Animal {} \
                      class A { int f(Animal a) { return 1; } int f(Dog d) { return 2; } \
                      void m() { f|(new Dog()); } }";
        assert_eq!(
            call_definition(source).expect("f(Dog)").signature,
            "int f(Dog d)"
        );
    }

    #[test]
    fn definition_on_a_variadic_candidate_separates_on_its_fixed_prefix_only() {
        // The fixed prefix is checked, so `new Foo()` in first position picks
        // the `Foo` overload even though both are variadic.
        let prefix = "class Foo {} class Bar {} \
                      class A { int f(Foo p, int... xs) { return 1; } \
                      int f(Bar p, int... xs) { return 2; } \
                      void m() { f|(new Foo(), 1); } }";
        assert_eq!(
            call_definition(prefix).expect("the Foo overload").signature,
            "int f(Foo p, int... xs)"
        );
        // The tail is not checked, so the same separation one position later
        // answers nothing. That is the documented ceiling in `apply.rs`, not a
        // difference the two positions have in Java.
        let tail = "class Foo {} class Bar {} \
                    class A { int f(int i, Foo... ps) { return 1; } \
                    int f(int i, Bar... ps) { return 2; } \
                    void m() { f|(1, new Foo()); } }";
        assert!(call_definition(tail).is_none());
    }

    #[test]
    fn a_sole_candidate_answers_even_when_the_argument_does_not_fit_it() {
        // Elimination runs only when more than one candidate is left, so the
        // single-candidate path never types an argument — both because it is
        // the common one and because a buffer mid-edit is far more often
        // between two keystrokes than wrong about which method it means.
        let source = "class Foo {} \
                      class A { int f(Foo p) { return 1; } void m() { f|(1); } }";
        assert_eq!(
            call_definition(source)
                .expect("the only method of that name")
                .signature,
            "int f(Foo p)"
        );
    }

    #[test]
    fn definition_on_a_call_no_overload_can_take_answers_nothing() {
        // Both candidates excluded is not one survivor: `f(1)` against
        // `f(Foo)` and `f(Bar)` leaves nothing standing, and nothing is the
        // answer rather than whichever was eliminated last.
        let source = "class Foo {} class Bar {} \
                      class A { int f(Foo p) { return 1; } int f(Bar p) { return 2; } \
                      void m() { f|(1); } }";
        assert!(call_definition(source).is_none());
    }

    #[test]
    fn an_inherited_overload_the_argument_fits_beats_the_subtypes_own_one_that_it_does_not() {
        // Phase order, and it is the reason elimination runs *before* the
        // own-shadows-inherited rule: `Sub.f(Bar)` does not override
        // `Base.f(Foo)` — different signatures make them overloads — so
        // `f(new Foo())` means `Base`'s in Java. Shadowing first would have
        // taken `Sub`'s and jumped to the wrong method.
        let source = "class Foo {} class Bar {} \
                      class Base { int f(Foo p) { return 1; } } \
                      class Sub extends Base { int f(Bar p) { return 2; } } \
                      class A { Sub s; void m() { s.f|(new Foo()); } }";
        assert_eq!(
            member_definition(source)
                .expect("the inherited overload")
                .signature,
            "int f(Foo p)"
        );
        let own = source.replace("new Foo()", "new Bar()");
        assert_eq!(
            member_definition(&own)
                .expect("the subtype's own overload")
                .signature,
            "int f(Bar p)"
        );
    }

    #[test]
    fn a_subtype_overload_does_not_hide_a_supertype_overload_of_a_different_parameter_type() {
        // The own-shadows-inherited rule narrows by *where* a member was
        // declared, and that is only a demonstration when the own member
        // overrides the inherited one — which needs the parameter types to
        // match, not merely the arity to fit. `Sub.f(Bar)` and `Base.f(Foo)`
        // are an overload set, so with an argument that separates neither the
        // answer is nothing; taking `Sub`'s because it is nearer would be a
        // pick, and Java's answer depends on `u`'s real type.
        let source = "class Foo {} class Bar {} \
                      class Base { int f(Foo p) { return 1; } } \
                      class Sub extends Base { int f(Bar p) { return 2; } } \
                      class A { Sub s; void m(Unresolvable u) { s.f|(u); } }";
        assert!(member_definition(source).is_none());
        // Not vacuous: the same shape with an argument that does separate them
        // still answers, so the refusal above is the shadowing rule narrowing
        // and not elimination having stopped working.
        let separable = source.replace("(u)", "(new Bar())");
        assert_eq!(
            member_definition(&separable)
                .expect("the subtype's own overload")
                .signature,
            "int f(Bar p)"
        );
    }

    #[test]
    fn a_generic_override_the_subtype_pinned_down_is_no_longer_shadowed() {
        // The cost of demanding demonstrable equality, and a ceiling rather
        // than a bug: `A.m(Foo)` really does override `Base<Foo>.m(T)`, but
        // saying so needs the parameter's type substituted through
        // `Member::subs`, which `Member::parameter_types` does not do — so `T`
        // stays a type variable, the two parameter types are not equal, and
        // the access refuses. Substituting them is the upgrade named there.
        let source = "class Foo {} \
                      class Base<T> { int m(T t) { return 0; } } \
                      class A extends Base<Foo> { int m(Foo f) { return 1; } \
                      void go() { m|(new Foo()); } }";
        assert!(call_definition(source).is_none());
        // Not vacuous: drop the generic and the identical override shadows as
        // it always has.
        let concrete = source
            .replace("class Base<T> { int m(T t)", "class Base { int m(Foo t)")
            .replace("extends Base<Foo>", "extends Base");
        assert_eq!(
            call_definition(&concrete)
                .expect("the overriding method")
                .signature,
            "int m(Foo f)"
        );
    }

    #[test]
    fn a_this_argument_inside_an_anonymous_class_is_not_excluded_from_the_type_it_instantiates() {
        // Soundness, not narrowing: `FileSyntax` records an anonymous class's
        // instantiated type as its *name* rather than as an `extends`, so the
        // subtype walk cannot see that this `this` is a `Marker`. Answering
        // "not a subtype" there would exclude `f(Marker)` and leave `f(int)` —
        // which the primitive rule then rules out too, so the visible symptom
        // is a refusal here, but pair `f(Marker)` with an unresolvable
        // parameter type and it becomes a wrong jump. `may_be_subtype` answers
        // "could be" for an anonymous class instead.
        let source = "interface Marker {} \
                      class O { int f(Marker m) { return 1; } int f(int i) { return 2; } } \
                      class A { O o; void go() { \
                      Marker r = new Marker() { void x() { o.f|(this); } }; } }";
        assert_eq!(
            member_definition(source)
                .expect("the Marker overload")
                .signature,
            "int f(Marker m)"
        );
    }

    #[test]
    fn a_qualified_call_separates_by_argument_type_exactly_as_an_unqualified_one_does() {
        // One filter, two paths: `sole_candidate` is shared, so the receiver
        // makes no difference to which overload the arguments leave standing.
        let source = "class Foo {} class Bar {} \
                      class O { int f(Foo p) { return 1; } int f(Bar p) { return 2; } } \
                      class A { O o; void m() { o.f|(new Bar()); } }";
        assert_eq!(
            member_definition(source)
                .expect("the Bar overload")
                .signature,
            "int f(Bar p)"
        );
    }

    fn external_type(marked: &str, world: &World) -> Option<String> {
        let f = fixture(marked);
        external_type_at(&f.document, &f.tree, &f.files(), world, f.offset).map(|found| found.fqn)
    }

    #[test]
    fn a_name_no_reachable_file_declares_resolves_to_the_type_the_world_holds() {
        let world = World::holding(&["com.example.Greeter"]);
        let source = "class A { Gree|ter g; }";
        assert_eq!(
            external_type(source, &world),
            Some("com.example.Greeter".to_owned())
        );
    }

    #[test]
    fn the_reference_span_is_the_name_the_question_was_asked_about() {
        // The one span this answer carries, and the one a hover highlights:
        // it is in the buffer the request named, unlike everything else about
        // a type nothing here can read.
        let world = World::holding(&["com.example.Greeter"]);
        let marked = "class A { Gree|ter g; }";
        let f = fixture(marked);
        let found = external_type_at(&f.document, &f.tree, &f.files(), &world, f.offset)
            .expect("the name resolves");
        let source = marked.replace('|', "");
        let at = source.find("Greeter").expect("the name") as u32;
        assert_eq!(found.reference.start_byte, at);
        assert_eq!(found.reference.end_byte, at + "Greeter".len() as u32);
    }

    /// Each segment of a qualified type name resolves the dotted text up to
    /// it; a first segment the file binds as a type — a type parameter —
    /// refuses the rest.
    #[test]
    fn a_qualified_names_segment_resolves_the_text_up_to_it() {
        let world = World::holding(&["java.util.Map", "java.util.Map.Entry"]);
        for (marked, fqn) in [
            ("class A { java.util.M|ap m; }", Some("java.util.Map")),
            (
                "class A { java.util.Map.En|try e; }",
                Some("java.util.Map.Entry"),
            ),
            (
                "import java.util.Map; class A { Map.En|try e; }",
                Some("java.util.Map.Entry"),
            ),
            ("import java.util.M|ap; class A {}", Some("java.util.Map")),
            ("class A { java.ut|il.Map m; }", None),
            ("class A<Map> { Map.En|try e; }", None),
        ] {
            assert_eq!(external_type(marked, &world).as_deref(), fqn, "{marked}");
        }
    }

    #[test]
    fn a_name_the_file_itself_declares_is_left_to_the_file_local_query() {
        // Two answers for one position is the failure this refusal prevents:
        // `definition_at` already answers a name the buffer declares, with a
        // span and a signature this one could not produce.
        let world = World::holding(&["com.example.Greeter"]);
        assert_eq!(
            external_type("class Greeter {} class A { Gree|ter g; }", &world),
            None
        );
    }

    #[test]
    fn a_type_parameter_and_a_bound_local_are_both_refused() {
        let world = World::holding(&["com.example.T", "com.example.value"]);
        // A type parameter shadows everything, exactly as `resolve_written`
        // has it.
        assert_eq!(external_type("class Box<T> { |T value; }", &world), None);
        // And a name bound in the variable namespace is not a type reference
        // at all.
        assert_eq!(
            external_type(
                "class Box { int value; int m() { return val|ue; } }",
                &world
            ),
            None
        );
    }

    #[test]
    fn an_ambiguous_name_answers_nothing_rather_than_one_of_the_two() {
        // Two reachable types of one simple name. The invariant, at the one
        // place in this slice where a guess would be cheap.
        let world = World::holding(&["com.example.Greeter", "com.other.Greeter"]);
        assert_eq!(external_type("class A { Gree|ter g; }", &world), None);
    }

    #[test]
    fn a_member_selection_is_not_a_type_reference() {
        // `b.width` names a member, and `name_at` refuses it for the reason
        // `definition_at` does: answering it from a same-named type would be
        // a jump into the wrong declaration.
        let world = World::holding(&["com.example.width"]);
        assert_eq!(
            external_type(
                "class Box { int width; } class A { void m() { Box b; b.wid|th; } }",
                &world
            ),
            None
        );
    }

    #[test]
    fn a_constructor_call_names_the_type_it_instantiates() {
        let world = World::holding(&["com.example.Greeter"]);
        assert_eq!(
            external_type("class A { void m() { new Gree|ter(); } }", &world),
            Some("com.example.Greeter".to_owned())
        );
    }

    /// A world of three types a file imports statically from: `Limits` and
    /// `Other` each declare a static `MAX`, and `Lists` a static `of`, a
    /// static `hashCode` and an instance `go`.
    fn imported_world() -> World {
        let string = ExternalType::Class("java.lang.String".into());
        let int = ExternalType::Primitive(crate::Primitive::Int);
        let fixed = |mut member: ExternalMember| {
            member.is_static = true;
            member
        };
        World::default()
            .with("java.lang.Object", &[], vec![])
            .with(
                "java.lang.String",
                &["java.lang.Object"],
                vec![method("java.lang.String", "length", int.clone(), vec![])],
            )
            .with(
                "p.Limits",
                &["java.lang.Object"],
                vec![
                    fixed(field("p.Limits", "MAX", string.clone())),
                    fixed(field("p.Limits", "log", string.clone())),
                ],
            )
            .with(
                "p.Other",
                &["java.lang.Object"],
                vec![fixed(field("p.Other", "MAX", int.clone()))],
            )
            .with(
                "p.Lists",
                &["java.lang.Object"],
                vec![
                    fixed(method(
                        "p.Lists",
                        "of",
                        string.clone(),
                        vec![string.clone()],
                    )),
                    fixed(method("p.Lists", "hashCode", int.clone(), vec![])),
                    method("p.Lists", "go", int.clone(), vec![]),
                    method("p.Lists", "of", int.clone(), vec![]),
                ],
            )
            .with(
                "p.Nums",
                &["java.lang.Object"],
                vec![fixed(method("p.Nums", "of", int.clone(), vec![int]))],
            )
            .with(
                "p.Base",
                &["java.lang.Object"],
                vec![field("p.Base", "MAX", string.clone())],
            )
            .with("p.Hidden", &["java.lang.Object"], {
                let mut max = field("p.Hidden", "MAX", string);
                max.is_public = false;
                vec![max]
            })
    }

    /// Who declares what a static import brings in at the cursor, if
    /// anything does.
    fn imported(source: &str) -> Option<String> {
        let f = fixture(source);
        external_member_at(
            &f.document,
            &f.tree,
            &f.files(),
            &imported_world(),
            f.offset,
        )
        .map(|found| format!("{}.{}", found.owner, found.name))
    }

    /// A bare `log` is the logger Lombok writes into the innermost type with
    /// a field of the name, and its chain is typed through it.
    #[test]
    fn a_lombok_logger_is_the_innermost_types_generated_field() {
        let logger = ExternalType::Class("org.slf4j.Logger".into());
        let generated = |owner: &str| {
            let mut log = field(owner, "log", logger.clone());
            log.is_static = true;
            log.is_public = false;
            log
        };
        let world = imported_world()
            .with(
                "org.slf4j.Logger",
                &["java.lang.Object"],
                vec![method(
                    "org.slf4j.Logger",
                    "info",
                    ExternalType::Void,
                    vec![ExternalType::Class("java.lang.String".into())],
                )],
            )
            .with(
                "p.Logged",
                &["java.lang.Object"],
                vec![field("p.Logged", "log", logger.clone())],
            )
            .generating("A", generated("A"))
            .generating("A.B", generated("A.B"));
        // A `lombok.config` naming it `audit`, for `A` alone.
        let mut audit = generated("A");
        audit.name = "audit".into();
        let renamed = imported_world()
            .generating("A", audit)
            .generating("A.Base", generated("A.Base"));
        let found = |source: &str| {
            let f = fixture(&format!("import lombok.extern.slf4j.Slf4j;\n{source}"));
            external_member_at(&f.document, &f.tree, &f.files(), &world, f.offset)
                .map(|found| format!("{}.{}", found.owner, found.name))
        };
        for (source, expected) in [
            (
                "@Slf4j class A { void m() { lo|g.info(\"x\"); } }",
                Some("A.log"),
            ),
            (
                "@Slf4j class A { void m() { log.in|fo(\"x\"); } }",
                Some("org.slf4j.Logger.info"),
            ),
            (
                "@Slf4j class A { static class C { void m() { lo|g.info(\"x\"); } } }",
                Some("A.log"),
            ),
            (
                "@Slf4j class A { @Slf4j static class B { void m() { lo|g.info(\"x\"); } } }",
                Some("A.B.log"),
            ),
            // `@CustomLog`'s field is not generated, and may be `log`.
            (
                "@Slf4j class A { @lombok.CustomLog static class C { void m() { lo|g.info(\"x\"); } } }",
                None,
            ),
            // A written field of the name is found first, inherited here.
            (
                "@Slf4j class A { static class C extends p.Logged { void m() { lo|g.info(\"x\"); } } }",
                Some("p.Logged.log"),
            ),
            (
                "@Slf4j class A { void m(Object log) { lo|g.toString(); } }",
                None,
            ),
            (
                "@Deprecated class A { void m() { lo|g.info(\"x\"); } }",
                None,
            ),
        ] {
            assert_eq!(found(source).as_deref(), expected, "{source}");
        }
        let found = |source: &str| {
            let f = fixture(&format!("import lombok.extern.slf4j.Slf4j;\n{source}"));
            external_member_at(&f.document, &f.tree, &f.files(), &renamed, f.offset)
                .map(|found| format!("{}.{}", found.owner, found.name))
        };
        assert_eq!(
            found("@Slf4j class A { static class C { void m() { aud|it.toString(); } } }")
                .as_deref(),
            Some("A.audit")
        );
        // A logging type whose field the index does not hold — a record's, or
        // one not read yet — may be the one called `audit`.
        assert_eq!(
            found("@Slf4j class A { @Slf4j static class D { void m() { aud|it.toString(); } } }"),
            None
        );
        // Nor is a supertype's logger its own.
        assert_eq!(
            found(
                "@Slf4j class A { @Slf4j static class Base {} \
                 @Slf4j static class D extends Base { void m() { aud|it.toString(); } } }"
            ),
            None
        );
    }

    #[test]
    fn a_statically_imported_field_or_method_is_reached_and_types_its_chain() {
        assert_eq!(
            imported("import static p.Limits.MAX;\nclass A { Object m() { return MA|X; } }")
                .as_deref(),
            Some("p.Limits.MAX")
        );
        assert_eq!(
            imported("import static p.Limits.MAX;\nclass A { int m() { return MAX.len|gth(); } }")
                .as_deref(),
            Some("java.lang.String.length"),
            "the chain is typed through it"
        );
        assert_eq!(
            imported("import static p.Lists.of;\nclass A { void m() { o|f(\"x\"); } }").as_deref(),
            Some("p.Lists.of")
        );
        assert_eq!(
            imported(
                "import static p.Lists.of;\nclass A { int m() { return of(\"x\").len|gth(); } }"
            )
            .as_deref(),
            Some("java.lang.String.length")
        );
        assert_eq!(
            imported("import static p.Limits.*;\nclass A { Object m() { return MA|X; } }")
                .as_deref(),
            Some("p.Limits.MAX"),
            "on demand, where one type holds the name"
        );
        assert_eq!(
            imported(
                "import static p.Limits.MAX;\nimport static p.Other.*;\n\
                 class A { Object m() { return MA|X; } }"
            )
            .as_deref(),
            Some("p.Limits.MAX"),
            "a single-static import shadows an on-demand one"
        );
        assert_eq!(
            imported(
                "import static p.Lists.of;\nimport static p.Limits.*;\n\
                 class A { void m() { o|f(\"x\"); } }"
            )
            .as_deref(),
            Some("p.Lists.of"),
            "beside an on-demand owner with no method of the name"
        );
        assert_eq!(
            imported(
                "import lombok.RequiredArgsConstructor;\nimport lombok.extern.slf4j.Slf4j;\n\
                 import org.springframework.stereotype.Service;\nimport static p.Limits.MAX;\n\
                 import lombok.Getter;\n\
                 @Service @Slf4j @RequiredArgsConstructor\n\
                 class A { @Getter private final String s; Object m() { return MA|X; } }"
            )
            .as_deref(),
            Some("p.Limits.MAX"),
            "past annotations that can add no member of the name"
        );
        assert_eq!(
            imported(
                "import lombok.Getter;\nimport org.springframework.stereotype.Service;\n\
                 import static p.Lists.of;\n@Service class A { @Deprecated String s; void m() { o|f(\"x\"); } }"
            )
            .as_deref(),
            Some("p.Lists.of"),
            "an annotation no generator rewrites a class for"
        );
        assert_eq!(
            imported(
                "import static p.Limits.MAX;\nclass A { class B { Object m() { return MA|X; } } }"
            )
            .as_deref(),
            Some("p.Limits.MAX"),
            "past every enclosing type"
        );
    }

    #[test]
    fn a_static_import_is_reached_only_where_nothing_closer_can_mean_the_name() {
        for (source, why) in [
            (
                "import static p.Limits.MAX;\nclass A { String MAX; Object m() { return MA|X; } }",
                "the file's own field",
            ),
            (
                "import static p.Lists.of;\nclass A { String of(String s) { return s; } \
                 void m() { o|f(\"x\"); } }",
                "the file's own method",
            ),
            (
                "import static p.Lists.go;\nclass A { void m() { g|o(); } }",
                "an instance method is never imported",
            ),
            (
                "import static p.Lists.of;\nclass A { void m() { o|f(); } }",
                "nor an instance overload of a static one",
            ),
            (
                "import static p.Limits.MAX;\nimport static p.Other.MAX;\n\
                 class A { Object m() { return MA|X; } }",
                "two single-static imports of the name",
            ),
            (
                "import static p.Lists.of;\nimport static p.Nums.*;\n\
                 class A { void m() { o|f(\"x\"); } }",
                "an on-demand method of the name competes with a single-static one",
            ),
            (
                "import lombok.AllArgsConstructor;\nimport static p.Lists.of;\n\
                 @AllArgsConstructor(staticName = \"of\") class A { String s; \
                 A m() { return o|f(\"x\"); } }",
                "a type an annotation may add the method to",
            ),
            (
                "import lombok.Getter;\nimport static p.Lists.of;\n\
                 class A { @Getter String s; void m() { o|f(\"x\"); } }",
                "a method Lombok may generate from an annotated member",
            ),
            (
                "import lombok.extern.slf4j.Slf4j;\nimport static p.Limits.log;\n\
                 @Slf4j class A { Object m() { return lo|g; } }",
                "the field a log annotation generates",
            ),
            (
                "import lombok.*;\nimport static p.Limits.log;\n\
                 @CustomLog class A { { Object o = lo|g; } }",
                "on a type with no member to carry it",
            ),
            (
                "import lombok.Getter;\nimport static p.Lists.of;\n\
                 @Getter class Base { String s; }\n\
                 class A extends Base { void m() { o|f(\"x\"); } }",
                "on a supertype the file declares",
            ),
            (
                "import static p.Limits.MAX;\n\
                 @lombok.experimental.FieldNameConstants class A { Object m() { return MA|X; } }",
                "a constant Lombok may generate",
            ),
            (
                "import static p.Limits.*;\nimport static p.Other.*;\n\
                 class A { Object m() { return MA|X; } }",
                "two on-demand owners holding it",
            ),
            (
                "import static p.Limits.MAX;\nclass A extends Missing { Object m() { return MA|X; } }",
                "an enclosing type whose list may be short",
            ),
            (
                "import static p.Lists.hashCode;\nclass A { int m() { return hash|Code(); } }",
                "a name `Object` declares",
            ),
            (
                "import static p.Limits.MAX;\nenum E { X { Object f() { return MA|X; } } }",
                "inside an enum constant's body",
            ),
            (
                "import lombok.experimental.Delegate;\nimport static p.Limits.MAX;\n\
                 class A { Object m() { return MA|X; } }",
                "a file that can name a member rewriter",
            ),
            (
                "import static p.Limits.MAX;\nclass A { Object m() { return new Object() { \
                 Object n() { return MA|X; } }; } }",
                "inside an anonymous class",
            ),
            (
                "import static p.Limits.MAX;\nclass A { @interface N { String v() default MA|X; } }",
                "inside an annotation type",
            ),
        ] {
            assert_eq!(imported(source), None, "{why}");
        }
        assert_eq!(
            imported(
                "import static p.Limits.MAX;\nclass A extends p.Base { Object m() { return MA|X; } }"
            )
            .as_deref(),
            Some("p.Base.MAX"),
            "an inherited field is closer than the import, and is what the name means"
        );
        for (source, why) in [
            (
                "import static p.Limits.MAX;\nclass A extends p.Hidden { Object m() { return MA|X; } }",
                "a field that is not public may be another package's, which is not inherited",
            ),
            (
                "class O { class A extends p.Hidden { Object m() { return MA|X; } } }",
                "nor where a type around it may hold the name",
            ),
            (
                "import static p.Limits.MAX;\n@N(MA|X) class A extends p.Base { }",
                "a type's own annotation is outside its body",
            ),
        ] {
            assert_eq!(imported(source).as_deref(), None, "{why}");
        }
        assert_eq!(
            imported("class A extends p.Hidden { Object m() { return MA|X; } }").as_deref(),
            Some("p.Hidden.MAX"),
            "with nothing else the name may mean"
        );
    }

    fn constructor(marked: &str, externals: &dyn Externals) -> Option<SelectedConstructor> {
        let f = fixture(marked);
        constructor_at(&f.document, &f.tree, &f.files(), externals, f.offset, true)
    }

    /// The parameter list of the local constructor `new` selected.
    fn selected_local(marked: &str, externals: &dyn Externals) -> Option<String> {
        let found = constructor(marked, externals)?;
        let ConstructorTarget::Local(definition) = found.target else {
            panic!("a local constructor")
        };
        Some(definition.signature)
    }

    fn ctor(owner: &str, name: &str, parameters: Vec<(ExternalType, &str)>) -> ExternalMember {
        let mut member = method(
            owner,
            name,
            ExternalType::Unresolved(String::new()),
            parameters.iter().map(|(ty, _)| ty.clone()).collect(),
        );
        member.kind = javelin_parser::DeclKind::Constructor;
        member.parameter_names = parameters
            .iter()
            .map(|(_, name)| (*name).to_owned())
            .collect();
        member
    }

    const BOXES: &str = "class Box { Box(int n) {} Box(String s) {} Box(int a, int b) {} void put(int a, int b) {} } ";

    #[test]
    fn a_new_selects_the_constructor_its_arguments_reach() {
        let two = format!("{BOXES}class A {{ void m() {{ new B|ox(1, 2); }} }}");
        let found = constructor(&two, &NoExternals).expect("the two-argument constructor");
        assert_eq!(found.parameter_names, ["a", "b"]);
        let ConstructorTarget::Local(definition) = &found.target else {
            panic!("a local constructor")
        };
        assert_eq!(definition.kind, DefinitionKind::Constructor);
        assert_eq!(definition.signature, "Box(int a, int b)");
        let text = fixture(&two).document.text().to_owned();
        let reference = definition.reference;
        assert_eq!(
            &text[reference.start_byte as usize..reference.end_byte as usize],
            "Box"
        );
        // Two of one arity are told apart by the argument's type.
        let string = format!("{BOXES}class A {{ void m() {{ new B|ox(\"x\"); }} }}");
        assert_eq!(
            selected_local(&string, &string_world()).as_deref(),
            Some("Box(String s)")
        );
        // ...and an argument nothing types tells them apart not at all.
        let unknown = format!("{BOXES}class A {{ void m() {{ new B|ox(q); }} }}");
        assert!(constructor(&unknown, &NoExternals).is_none());
        // The most specific of two that both take an `int`.
        let widening = "class Box { Box(int n) {} Box(long n) {} } \
                        class A { void m() { new B|ox(1); } }";
        assert_eq!(
            selected_local(widening, &NoExternals).as_deref(),
            Some("Box(int n)")
        );
        // Ranked though the type's member walk is short: its constructors are
        // not inherited, so the list is whole whatever it extends.
        let unresolved = "class Box extends Missing { Box(int n) {} Box(long n) {} } \
                          class A { void m() { new B|ox(1); } }";
        assert_eq!(
            selected_local(unresolved, &NoExternals).as_deref(),
            Some("Box(int n)")
        );
    }

    /// `this(…)` selects among the class's own constructors, `super(…)` among
    /// its written superclass's, and an enum constant among the enum's.
    #[test]
    fn an_explicit_constructor_call_selects_as_a_new_does() {
        let this = "class Box { Box(int n) {} Box(long n) {} Box() { th|is(1); } }";
        assert_eq!(
            selected_local(this, &NoExternals).as_deref(),
            Some("Box(int n)")
        );
        let generic = "class Box<T> { Box(T t, int n) {} Box(T t) { th|is(t, 1); } }";
        assert_eq!(
            selected_local(generic, &NoExternals).as_deref(),
            Some("Box(T t, int n)")
        );
        let sup = "class Base { Base(int n) {} Base() {} } \
                   class Box extends Base { Box() { sup|er(1); } }";
        assert_eq!(
            selected_local(sup, &NoExternals).as_deref(),
            Some("Base(int n)")
        );
        let constant = "enum Day { MO|N(1) { }, TUE; Day(int n) {} Day() {} }";
        assert_eq!(
            selected_local(constant, &NoExternals).as_deref(),
            Some("Day(int n)")
        );
        let bare = "enum Day { MON(1), T|UE; Day(int n) {} Day() {} }";
        assert_eq!(selected_local(bare, &NoExternals).as_deref(), Some("Day()"));
        // What the constant's name *is* is the constant.
        let f = fixture(constant);
        assert!(
            constructor_at(
                &f.document,
                &f.tree,
                &f.files(),
                &NoExternals,
                f.offset,
                false
            )
            .is_none()
        );
        // None written is the implicit one, which is no target.
        assert!(constructor("enum Day { MO|N; }", &NoExternals).is_none());
        // No written superclass is `Object`'s, which nothing here declares.
        assert!(constructor("class Box { Box() { sup|er(); } }", &NoExternals).is_none());
    }

    #[test]
    fn a_local_classs_constructors_are_not_picked() {
        // Its parameter types may name a member type its supertypes declare.
        let source = "class A { void m() { class L { L(int n) {} L(long n) {} } new L|(1); } }";
        assert!(constructor(source, &NoExternals).is_none());
    }

    /// JLS 15.12.2.1 drops a constructor the call cannot see before fixed
    /// arity is preferred, so one that may be hidden does not push out a
    /// variadic sibling: Java picks `A(S...)` here.
    #[test]
    fn a_constructor_the_call_may_not_see_does_not_beat_a_variadic_one() {
        let hidden = "class S {} class A { private A(S s) {} A(S... s) {} } \
                      class B { Object m(S s) { return new A|(s); } }";
        assert!(constructor(hidden, &NoExternals).is_none());
        let seen = hidden.replace("private A(S s)", "A(S s)");
        assert_eq!(
            selected_local(&seen, &NoExternals).as_deref(),
            Some("A(S s)")
        );
    }

    #[test]
    fn a_new_reaches_only_the_types_own_written_constructors() {
        // Never an inherited one (JLS 8.8).
        let inherited = "class Base { Base(int n) {} } class Box extends Base { Box() {} } \
                         class A { void m() { new B|ox(1); } }";
        assert!(constructor(inherited, &NoExternals).is_none());
        // None written is the implicit default, which is no target.
        let implicit = "class Box { } class A { void m() { new B|ox(); } }";
        assert!(constructor(implicit, &NoExternals).is_none());
        // Lombok may add one beside the written one, so the lone written
        // constructor proves nothing.
        let lombok = "@AllArgsConstructor class Box { int n; Box() {} } \
                      class A { void m() { new B|ox(); } }";
        assert!(constructor(lombok, &NoExternals).is_none());
        let plain = "@Deprecated class Box { int n; Box() {} } \
                     class A { void m() { new B|ox(); } }";
        assert_eq!(
            selected_local(plain, &NoExternals).as_deref(),
            Some("Box()")
        );
    }

    #[test]
    fn an_anonymous_class_passes_its_arguments_to_the_constructor() {
        let source = "abstract class Box { Box(int n) {} Box() {} } \
                      class A { void m() { new B|ox(1) { }; } }";
        assert_eq!(
            selected_local(source, &NoExternals).as_deref(),
            Some("Box(int n)")
        );
    }

    #[test]
    fn a_new_inside_a_local_or_anonymous_class_selects_nothing() {
        // A member type a supertype declares may mean another `Box` there.
        let source = format!(
            "{BOXES}class A {{ void m() {{ class L {{ void n() {{ new B|ox(1, 2); }} }} }} }}"
        );
        assert!(constructor(&source, &NoExternals).is_none());
    }

    #[test]
    fn the_pick_records_the_names_it_read() {
        let found = constructor(
            &format!("{BOXES}class A {{ void m() {{ new B|ox(\"x\"); }} }}"),
            &string_world(),
        )
        .expect("the String constructor");
        assert!(found.written.contains(&"Box".to_owned()));
        assert!(found.written.contains(&"String".to_owned()));
    }

    #[test]
    fn an_external_types_constructor_is_selected_from_the_worlds_whole_list() {
        let int = || ExternalType::Primitive(crate::Primitive::Int);
        let string = || ExternalType::Class("java.lang.String".to_owned());
        let world = string_world().with("p.Box", &[], vec![]).constructing(
            "p.Box",
            vec![
                ctor("p.Box", "Box", vec![(int(), "size")]),
                ctor("p.Box", "Box", vec![(string(), "label")]),
            ],
        );
        let found = constructor("class A { void m() { new p.B|ox(\"x\"); } }", &world)
            .expect("the String constructor");
        let ConstructorTarget::External(summary) = &found.target else {
            panic!("an external constructor")
        };
        assert_eq!(summary.signature, "Box(String label)");
        assert_eq!(found.parameter_names, ["label"]);
        // A world that cannot vouch for the list answers nothing.
        let unlisted = string_world().with("p.Box", &[], vec![]);
        assert!(constructor("class A { void m() { new p.B|ox(1); } }", &unlisted).is_none());
    }

    #[test]
    fn a_workspace_constructor_read_where_its_file_does_not_settle_it_is_not_picked() {
        let int = || ExternalType::Primitive(crate::Primitive::Int);
        let mut uncertain = ctor("p.Box", "Box", vec![(int(), "size")]);
        uncertain.site = Some(ExternalSite {
            doc_span: None,
            source: "/w/p/Box.java".to_owned(),
            name_start: 0,
            name_end: 3,
            name_range: Some(javelin_document::Range::new(
                javelin_document::Position::default(),
                javelin_document::Position::default(),
            )),
        });
        let world = |member: ExternalMember| {
            World::default()
                .with("p.Box", &[], vec![])
                .constructing("p.Box", vec![member])
        };
        let source = "class A { void m() { new p.B|ox(1); } }";
        assert!(constructor(source, &world(uncertain.clone())).is_none());
        uncertain.certain = true;
        assert!(constructor(source, &world(uncertain)).is_some());
    }
}
