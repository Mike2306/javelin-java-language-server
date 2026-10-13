//! Whether a member a call or a field access names is certainly absent from
//! its receiver's type — or, for an unqualified call, from every type around
//! it.
//!
//! The one caller of `walk_members`' wholeness that cannot afford to be wrong
//! in either direction but one: a missing answer here is a squiggle nobody
//! sees, and a wrong one is a reviewer told that working code calls a method
//! that does not exist. So the receiver is typed by rules narrower than
//! [`crate::type_of`]'s — anything they do not cover answers nothing — and
//! no overload is ever *picked* on the way, because picking one types its
//! arguments, which is `type_of` again.
//!
//! ponytail: a file-local receiver's list is short by the `Object` nobody
//! writes (see `walk_members`), so `this.x()` and a receiver of a type this
//! file declares are judged only where the type's chain ends in the world.
//! The upgrade is contributing the implicit supertype to the walk.

use javelin_document::TextDocument;
use javelin_parser::{
    DeclKind, Expr, Literal, SelectionKind, Span, SyntaxTree, member_selection_at,
    unqualified_call_at,
};

use crate::externals::Externals;
use crate::files::Files;
use crate::infer::{can_see_past, declared, enclosing_type, enclosing_types};
use crate::members::{Member, member_type, walk_members};
use crate::resolve::resolve_written;
use crate::types::{Type, strip_annotations};

/// How deep a receiver chain is followed, `var` hops included — the bound
/// `infer`'s own walk has, for the same reasons.
const MAX_DEPTH: u32 = 64;

/// A member a use names that no type it can mean has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingMember {
    pub name: String,
    pub span: Span,
    /// The receiver's type as a reader names it: fully qualified when no file
    /// the query reaches declares it, and its simple name when one does. For
    /// an unqualified call, the innermost type around it.
    pub receiver: String,
    /// The first segment of every type name the receiver chain resolved — or,
    /// for an unqualified call, every type around it wrote as a supertype —
    /// for a caller that knows which names inheritance shadows and this crate
    /// does not.
    pub written: Vec<String>,
    /// The chain's root, when it is a bare name read as a type — a static
    /// access — for a caller that knows which fields inheritance brings into
    /// scope.
    pub type_name: Option<String>,
    pub used: MemberUse,
}

/// How the missing member is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberUse {
    /// `receiver.name(…)`.
    Call,
    /// `receiver.name`. `through_type` when the receiver is a type name, so
    /// `name` may be a nested type, which no external list holds: a caller
    /// must rule out every frame of the receiver's closure declaring one.
    Field { through_type: bool },
    /// `name(…)`, searched in every type around it.
    Unqualified,
}

/// The member use whose name is at `offset` — a span
/// [`javelin_parser::member_name_spans`] gives — if what it names is
/// certainly absent.
///
/// `receiver.name(…)` and `receiver.name`: the receiver's type is certain,
/// both halves of its member list are whole, and neither holds a member
/// called `name` of any kind. An unqualified `name(…)`: no type around it has
/// a member called `name`, and every one's list is whole but for `Object`,
/// whose own names are left alone.
///
/// Any member of that name silences it — another arity, a field, a static
/// method through a value — because judging which of them a use can reach is
/// overload resolution, which `apply.rs` models only as far as exclusion. A
/// method reference is never judged.
pub fn missing_member_at(
    document: &TextDocument,
    tree: &SyntaxTree,
    files: &Files<'_>,
    externals: &dyn Externals,
    offset: u32,
) -> Option<MissingMember> {
    // ponytail: `member_selection_at` reads a `.` in the text before a name,
    // a comment's included, so `helpr()` on the line after `// done.` is a
    // selection with an opaque receiver and is not judged. The upgrade is
    // asking the tree whether the name is a `method_invocation` with no
    // `object` before falling back to the text.
    match member_selection_at(document, tree, offset) {
        Some(selection) => missing_selected(document, selection, files, externals),
        None => missing_unqualified(document, tree, files, externals, offset),
    }
}

fn missing_selected(
    document: &TextDocument,
    selection: javelin_parser::Selection,
    files: &Files<'_>,
    externals: &dyn Externals,
) -> Option<MissingMember> {
    let span = selection.name_span?;
    let after = document.text().get(span.end_byte as usize..)?;
    // The selection cuts a name where `char::is_alphanumeric` stops, and a
    // combining mark continues a Java identifier: the name must be followed
    // by its argument list, or by something no identifier continues with.
    let mut used = match selection.kind {
        SelectionKind::Call { .. } if after.trim_start().starts_with('(') => MemberUse::Call,
        SelectionKind::Value
            if after.chars().next().is_none_or(|next| {
                next.is_ascii() && !(next.is_ascii_alphanumeric() || matches!(next, '_' | '$'))
            }) =>
        {
            MemberUse::Field {
                through_type: false,
            }
        }
        _ => return None,
    };
    let mut chain = Chain::default();
    let receiver = certain(&selection.receiver, files, externals, 0, &mut chain)?;
    if let MemberUse::Field { through_type } = &mut used {
        *through_type =
            matches!(selection.receiver, Expr::Name { .. }) && chain.type_name.is_some();
        // ponytail: a file-local type's nested types are in its walk, and its
        // external frames' are not, so a field through a type name this file
        // declares is not judged. The upgrade is nested types in the closure.
        if *through_type && !matches!(receiver, Type::External { .. }) {
            return None;
        }
    }
    let label = match &receiver {
        Type::External { fqn, .. } => fqn.clone(),
        Type::Declared { name, .. } => name.clone(),
        _ => return None,
    };
    for statics in [false, true] {
        let walk = walk_members(&receiver, files, externals, statics);
        if !walk.closed
            || walk
                .members
                .iter()
                .any(|member| member.name() == selection.name)
        {
            return None;
        }
    }
    Some(MissingMember {
        name: selection.name,
        span,
        receiver: label,
        written: chain.written,
        type_name: chain.type_name,
        used,
    })
}

/// An unqualified `name(…)` no type around it can mean — JLS 15.12.1's search
/// run to its end without finding a class to search.
fn missing_unqualified(
    document: &TextDocument,
    tree: &SyntaxTree,
    files: &Files<'_>,
    externals: &dyn Externals,
    offset: u32,
) -> Option<MissingMember> {
    let Expr::Call {
        name,
        name_span,
        receiver: None,
        ..
    } = unqualified_call_at(document, tree, offset)?
    else {
        return None;
    };
    if crate::queries::OBJECT_METHODS.contains(&name.as_str()) {
        return None;
    }
    let around = enclosing_types(files, name_span.start_byte);
    let innermost = around.first()?;
    let mut chain = Chain::default();
    for decl in &around {
        if !can_see_past(decl.kind) {
            return None;
        }
        let ty = declared(files, decl);
        for statics in [false, true] {
            let walk = walk_members(&ty, files, externals, statics);
            if !walk.whole_but_object || walk.members.iter().any(|member| member.name() == name) {
                return None;
            }
        }
        record_supertypes(&ty, files, externals, &mut chain)?;
    }
    Some(MissingMember {
        name,
        span: name_span,
        receiver: innermost.name.clone(),
        written: chain.written,
        type_name: None,
        used: MemberUse::Unqualified,
    })
}

/// What typing a receiver chain resolved, for the caller's own checks.
#[derive(Default)]
struct Chain {
    written: Vec<String>,
    type_name: Option<String>,
    /// The file-local types whose supertypes are recorded, by name offset.
    visited: Vec<u32>,
}

/// The type of `expr` where it is certain, and `None` everywhere else.
///
/// A type variable, an array, a primitive or an unknown may come back, and
/// is refused one step later: `walk_members` shows no list whole for one.
fn certain(
    expr: &Expr,
    files: &Files<'_>,
    externals: &dyn Externals,
    depth: u32,
    chain: &mut Chain,
) -> Option<Type> {
    if depth > MAX_DEPTH {
        return None;
    }
    let syntax = files.current_syntax();
    let ty = match expr {
        Expr::Name { name, span } => {
            match files.current_scopes().resolve_value(span.start_byte, name) {
                Some(binding) => {
                    let decl = syntax.declaration_at(binding.name_span)?;
                    // From a nested type, a field the nested type inherits would
                    // shadow this one (JLS 6.4.1), and the scope tree does not
                    // know what a type inherits.
                    if matches!(decl.kind, DeclKind::Field | DeclKind::RecordComponent)
                        && enclosing_type(files, span.start_byte)
                            != enclosing_type(files, decl.name_span.start_byte)
                    {
                        return None;
                    }
                    // An implicit lambda parameter, an enum constant: nothing
                    // written to be certain of.
                    let written = decl.declared_type.as_deref()?;
                    // `String... args` is an array, which no call is judged on.
                    if decl.varargs {
                        return None;
                    }
                    if written == "var" {
                        // A for-each `var`, a resource's and a pattern's carry no
                        // initialiser, and are refused here.
                        certain(
                            decl.initializer.as_deref()?,
                            files,
                            externals,
                            depth + 1,
                            chain,
                        )?
                    } else {
                        written_type(written, files, externals, decl.name_span.start_byte, chain)
                    }
                }
                // Bound to no variable the file can see, so read as a type — the
                // caller checks the variables the file cannot see.
                None => {
                    chain.type_name = Some(name.clone());
                    written_type(name, files, externals, span.start_byte, chain)
                }
            }
        }
        Expr::This { span } => enclosing_type(files, span.start_byte),
        Expr::Super { span } => {
            let Type::Declared { decl, .. } = enclosing_type(files, span.start_byte) else {
                return None;
            };
            let enclosing = syntax.declaration_at(decl.span)?;
            let written = enclosing.extends.as_deref()?;
            written_type(
                written,
                files,
                externals,
                enclosing.name_span.start_byte,
                chain,
            )
        }
        Expr::New {
            type_text,
            anonymous: false,
            span,
            ..
        } => written_type(type_text, files, externals, span.start_byte, chain),
        Expr::Cast {
            type_text, span, ..
        } => written_type(type_text, files, externals, span.start_byte, chain),
        Expr::Call {
            receiver: Some(receiver),
            name,
            arguments,
            ..
        } => {
            let owner = certain(receiver, files, externals, depth + 1, chain)?;
            returned(&owner, name, arguments.len(), files, externals, chain)?
        }
        Expr::FieldAccess { receiver, name, .. } => {
            let owner = certain(receiver, files, externals, depth + 1, chain)?;
            held(&owner, name, files, externals, chain)?
        }
        // No name to shadow: a literal is `java.lang.String` whatever is in
        // scope.
        Expr::Literal {
            literal: Literal::String,
            ..
        } => crate::infer::string_type(externals),
        _ => return None,
    };
    record_supertypes(&ty, files, externals, chain)?;
    Some(ty)
}

/// Record the supertypes a file-local type writes, through every one this
/// file declares: each is resolved against the file, so each is a name
/// inheritance could shadow. `None` for a type another file declares, whose
/// names this query cannot read where they were written.
fn record_supertypes(
    ty: &Type,
    files: &Files<'_>,
    externals: &dyn Externals,
    chain: &mut Chain,
) -> Option<()> {
    let Type::Declared { decl, .. } = ty else {
        return Some(());
    };
    if decl.file != *files.current() {
        return None;
    }
    if chain.visited.contains(&decl.span.start_byte) {
        return Some(());
    }
    chain.visited.push(decl.span.start_byte);
    let declared = files.current_syntax().declaration_at(decl.span)?;
    for written in declared.extends.iter().chain(&declared.implements) {
        let supertype = written_type(
            written,
            files,
            externals,
            declared.name_span.start_byte,
            chain,
        );
        record_supertypes(&supertype, files, externals, chain)?;
    }
    Some(())
}

/// A written type, resolved where it was written, with its first segment
/// recorded.
fn written_type(
    written: &str,
    files: &Files<'_>,
    externals: &dyn Externals,
    at: u32,
    chain: &mut Chain,
) -> Type {
    record(written, &mut chain.written);
    resolve_written(written, files, externals, at)
}

/// The first segment of every type name `written` spells, its arguments'
/// included: a type argument can become a receiver — `l.get(0).x()` on a
/// `List<Entry>` — so its name is one inheritance could shadow too.
pub(crate) fn record(written: &str, out: &mut Vec<String>) {
    let names = strip_annotations(written.trim())
        .split(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '$' | '.')))
        .filter_map(|name| name.split('.').next())
        .filter(|first| !first.is_empty() && !matches!(*first, "extends" | "super"));
    out.extend(names.map(str::to_owned));
}

/// Every simple name `ty` names, its arguments' included.
pub(crate) fn record_resolved(ty: &Type, out: &mut Vec<String>) {
    match ty {
        Type::External { fqn, arguments, .. } => {
            out.extend(
                fqn.split('.')
                    .filter(|segment| segment.starts_with(char::is_uppercase))
                    .map(str::to_owned),
            );
            arguments
                .iter()
                .for_each(|argument| record_resolved(argument, out));
        }
        Type::Declared { arguments, .. } => {
            arguments
                .iter()
                .for_each(|argument| record_resolved(argument, out));
        }
        Type::Array(element) => record_resolved(element, out),
        _ => {}
    }
}

/// Every member of `owner` called `name` that `keep` admits, across both
/// halves — or `None` when either half may be short.
fn named<'a>(
    owner: &Type,
    name: &str,
    files: &Files<'a>,
    externals: &dyn Externals,
    keep: impl Fn(&Member<'a>) -> bool,
) -> Option<Vec<Member<'a>>> {
    let mut found = Vec::new();
    for statics in [false, true] {
        let walk = walk_members(owner, files, externals, statics);
        if !walk.closed {
            return None;
        }
        found.extend(
            walk.members
                .into_iter()
                .filter(|member| member.name() == name && keep(member)),
        );
    }
    Some(found)
}

/// The type `owner.name(…)` with `arity` arguments returns, when every
/// method it can mean returns the same one.
fn returned(
    owner: &Type,
    name: &str,
    arity: usize,
    files: &Files<'_>,
    externals: &dyn Externals,
    chain: &mut Chain,
) -> Option<Type> {
    let candidates = named(owner, name, files, externals, |member| {
        matches!(
            member.kind(),
            DeclKind::Method | DeclKind::AnnotationElement
        ) && (member.parameter_count() == arity
            || (member.is_variadic() && arity + 1 >= member.parameter_count()))
    })?;
    one_type(&candidates, files, externals, chain)
}

/// The type of `owner.name`, when every field of that name in the list has
/// the same one — a field a subtype hides with another type is neither.
fn held(
    owner: &Type,
    name: &str,
    files: &Files<'_>,
    externals: &dyn Externals,
    chain: &mut Chain,
) -> Option<Type> {
    let candidates = named(owner, name, files, externals, |member| {
        matches!(member.kind(), DeclKind::Field | DeclKind::EnumConstant)
    })?;
    one_type(&candidates, files, externals, chain)
}

/// The one type every candidate has, or `None` for none or several.
///
/// Never a workspace member's: its type is written in another file and
/// resolved there without the member types that file inherits or the names
/// its static imports bring in. A site that is a file on disk is the mark of
/// one — [`crate::ExternalSite::name_range`]'s contract.
fn one_type(
    candidates: &[Member<'_>],
    files: &Files<'_>,
    externals: &dyn Externals,
    chain: &mut Chain,
) -> Option<Type> {
    let workspace = |member: &&Member<'_>| {
        member
            .external()
            .and_then(|facts| facts.site.as_ref())
            .is_some_and(|site| site.name_range.is_some())
    };
    if candidates
        .iter()
        .filter(workspace)
        .any(|member| !member.external().is_some_and(|facts| facts.certain))
    {
        return None;
    }
    let mut types = candidates.iter().map(|member| {
        // A file-local member's type is written in this file and resolved
        // against it, so its name is one inheritance could shadow too. An
        // external member's names are recorded as resolved — a workspace
        // file, or a JDK member bound through a workspace edge, may have read
        // a generated name as another — for a gate that knows a generated one.
        if let Some(written) = member.declared_type() {
            record(written, &mut chain.written);
            member_type(member, files, externals)
        } else {
            let ty = member_type(member, files, externals);
            record_resolved(&ty, &mut chain.written);
            ty
        }
    });
    let first = types.next()?;
    types.all(|ty| ty == first).then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::externals::testing::{World, class, field, method};
    use crate::externals::{ExternalMember, ExternalType};
    use crate::files::FileId;
    use javelin_document::DocumentId;
    use javelin_parser::{FileSyntax, JavaParser, ScopeTree};

    const FILE: &str = "file:///A.java";
    const STRING: &str = "java.lang.String";
    const BOX: &str = "a.Box";

    fn int() -> ExternalType {
        ExternalType::Primitive(crate::Primitive::Int)
    }

    fn statik(mut member: ExternalMember) -> ExternalMember {
        member.is_static = true;
        member
    }

    /// `String` with a method, a field and a static method, and `a.Box` whose
    /// `get` is overloaded by arity with one return type and `pick` with two.
    fn world() -> World {
        World::default()
            .with(
                STRING,
                &[],
                vec![
                    method(STRING, "length", int(), vec![]),
                    field(STRING, "hash", int()),
                    statik(method(STRING, "valueOf", class(STRING), vec![int()])),
                ],
            )
            .with(
                BOX,
                &[],
                vec![
                    method(BOX, "get", class(STRING), vec![]),
                    method(BOX, "get", class(STRING), vec![int()]),
                    method(BOX, "pick", class(STRING), vec![int()]),
                    method(BOX, "pick", class(BOX), vec![class(STRING)]),
                    method(BOX, "wrap", class(BOX), vec![]),
                    method(BOX, "wrap", class(STRING), vec![int()]),
                    field(BOX, "shade", class(STRING)),
                    field("a.Base", "shade", class(BOX)),
                    field(BOX, "label", class(STRING)),
                    statik(method(BOX, "make", class(BOX), vec![])),
                ],
            )
            .with_partial(
                "a.Partial",
                &[],
                vec![method("a.Partial", "get", class(STRING), vec![])],
            )
    }

    /// Every call in `source` the query reports, by name.
    fn missing_in(source: &str, world: &World) -> Vec<String> {
        let document = TextDocument::new(DocumentId::new(FILE), 1, source);
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let scopes = ScopeTree::build(&document, &tree);
        let files = Files::single(FileId::new(FILE), &syntax, &scopes);
        javelin_parser::member_name_spans(&document, &tree, &javelin_core::CancellationToken::new())
            .expect("not cancelled")
            .into_iter()
            .filter_map(|span| missing_member_at(&document, &tree, &files, world, span.start_byte))
            .map(|found| found.name)
            .collect()
    }

    /// The names the first judged use's chain recorded.
    fn written_in(source: &str, world: &World) -> Vec<String> {
        let document = TextDocument::new(DocumentId::new(FILE), 1, source);
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let scopes = ScopeTree::build(&document, &tree);
        let files = Files::single(FileId::new(FILE), &syntax, &scopes);
        javelin_parser::member_name_spans(&document, &tree, &javelin_core::CancellationToken::new())
            .expect("not cancelled")
            .into_iter()
            .find_map(|span| missing_member_at(&document, &tree, &files, world, span.start_byte))
            .map(|found| found.written)
            .unwrap_or_default()
    }

    fn missing(body: &str) -> Vec<String> {
        missing_in(
            &format!("class A {{ String s; Box b; Partial p; void m() {{ {body} }} }}"),
            &world(),
        )
    }

    /// `@A(lenth = 1)`'s key is a member name `member_name_spans` gives, and
    /// no call or selection: it is never judged, absent from `A` or not.
    #[test]
    fn an_annotation_elements_key_is_never_judged() {
        let source = "@interface A { int length(); } class B { @A(lenth = 1) void m() {} }";
        assert_eq!(missing_in(source, &world()), Vec::<String>::new());
    }

    /// A string literal is a `java.lang.String` whatever the file declares,
    /// so a call on one is as certain as on a `String` local.
    #[test]
    fn a_call_on_a_string_literal_is_judged() {
        assert_eq!(missing("\"x\".lenth(); \"x\".length();"), vec!["lenth"]);
        assert_eq!(
            missing_in(
                "class String {} class A { void m() { \"x\".lenth(); } }",
                &world()
            ),
            vec!["lenth"],
            "a file-local `String` is not the literal's type"
        );
    }

    #[test]
    fn a_name_the_whole_receiver_lacks_is_reported() {
        assert_eq!(missing("s.nope();"), vec!["nope"]);
        assert_eq!(missing("Box.nope();"), vec!["nope"], "a static call");
    }

    #[test]
    fn a_name_the_receiver_has_in_any_form_is_not() {
        for call in [
            "s.length();",
            "s.length(1, 2);",
            "s.hash();",
            "s.valueOf(1);",
            "String.length();",
            "Box.make();",
        ] {
            assert_eq!(missing(call), Vec::<String>::new(), "{call}");
        }
    }

    #[test]
    fn a_receiver_whose_list_may_be_short_or_whose_type_is_uncertain_is_silent() {
        for call in [
            "p.nope();",
            "q.nope();",
            "int i = 0; i.nope();",
            "String[] a = null; a.nope();",
            "String a[] = null; a.nope();",
            "var v; v.nope();",
            "java.util.function.Function<String, String> f = x -> x.nope();",
            "(cond ? s : s).nope();",
        ] {
            assert_eq!(missing(call), Vec::<String>::new(), "{call}");
        }
    }

    /// `String... rest` is a `String[]` inside the method, whose `nope` no
    /// list of `String`'s members may settle.
    #[test]
    fn a_variadic_parameter_is_no_receiver_of_its_element_type() {
        assert_eq!(
            missing_in(
                "class A { void m(String... rest) { rest.nope(); } }",
                &world()
            ),
            Vec::<String>::new()
        );
    }

    /// Nobody writes `extends Object`, so a file-local type is judged only
    /// where its chain ends in the world — and then both halves of the
    /// external frame are read, the static one included.
    #[test]
    fn a_file_local_type_is_judged_only_through_a_chain_that_ends_in_the_world() {
        assert_eq!(
            missing_in(
                "class A { void m(L l, M m) { l.nope(); l.get(); m.nope(); } } \
                 class L extends Box {} class M {}",
                &world()
            ),
            vec!["nope"],
            "`l`'s chain ends at `a.Box`; `m`'s at an unwritten `Object`"
        );
    }

    #[test]
    fn a_type_variable_is_silent() {
        assert_eq!(
            missing_in(
                "class A<T> { T t; <U> void m(U u) { t.nope(); u.nope(); } }",
                &world()
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_var_is_as_certain_as_its_initialiser() {
        assert_eq!(missing("var x = s; x.nope();"), vec!["nope"]);
        assert_eq!(missing("var x = new Box(); x.nope();"), vec!["nope"]);
    }

    /// An anonymous class's own methods are callable on it and on a `var`
    /// that holds it.
    #[test]
    fn an_anonymous_creation_is_silent() {
        assert_eq!(
            missing("new Box() { void extra() {} }.extra();"),
            Vec::<String>::new()
        );
        assert_eq!(
            missing("var x = new Box() { void extra() {} }; x.extra();"),
            Vec::<String>::new()
        );
        assert_eq!(missing("new Box().nope();"), vec!["nope"]);
    }

    /// From a nested type, a field the nested type inherits would shadow the
    /// outer one, and inherited fields are not in the scope tree.
    #[test]
    fn a_field_used_from_a_nested_type_is_silent() {
        assert_eq!(
            missing_in(
                "class A { String s; void m() { s.nope(); } \
                 class Inner extends Box { void n() { s.nope(); } } }",
                &world()
            ),
            vec!["nope"],
            "only the use in the declaring type"
        );
    }

    /// A record's component is a field of the record, and from a nested
    /// type an inherited field of the same name would shadow it.
    #[test]
    fn a_record_component_used_from_a_nested_type_is_silent() {
        assert_eq!(
            missing_in(
                "record R(String s) { void m() { s.nope(); } \
                 class Inner extends Box { void n() { s.nope(); } } }",
                &world()
            ),
            vec!["nope"],
            "only the use in the record itself"
        );
    }

    /// A combining mark continues a Java identifier, and the name the text
    /// before the cursor is cut at is not the name the tree has.
    #[test]
    fn a_name_the_text_scan_cuts_short_is_silent() {
        assert_eq!(missing("s.cafe\u{301}();"), Vec::<String>::new());
    }

    #[test]
    fn a_chain_is_certain_only_through_one_return_type() {
        assert_eq!(missing("b.get().nope();"), vec!["nope"], "one return type");
        assert_eq!(
            missing("b.pick(1).nope();"),
            Vec::<String>::new(),
            "two overloads, two return types: nothing is picked"
        );
        assert_eq!(missing("b.label.nope();"), vec!["nope"], "one field");
        assert_eq!(
            missing("b.wrap(1).nope();"),
            vec!["nope"],
            "the arity leaves one return type"
        );
        assert_eq!(
            missing("b.shade.nope();"),
            Vec::<String>::new(),
            "a field and the one it hides with another type: neither is picked"
        );
        assert_eq!(
            missing("p.get().nope();"),
            Vec::<String>::new(),
            "a short list may hold another overload"
        );
    }

    /// `O.super` is `O`'s superclass, not the enclosing class's; an
    /// intersection cast has the members of every type it names; and
    /// `b.new String()` is a member type of `b`'s, not the `String` in scope.
    #[test]
    fn a_receiver_whose_type_the_text_before_the_dot_does_not_say_is_silent() {
        for (source, what) in [
            (
                "class O extends Box { class I extends String { void n() { O.super.get(); } } }",
                "a qualified super",
            ),
            (
                "class A { void m(Object o) { ((String & Runnable) o).run(); } }",
                "an intersection cast",
            ),
            (
                "class A { void m(Box b) { b.new String().nope(); } }",
                "a qualified creation",
            ),
        ] {
            assert_eq!(missing_in(source, &world()), Vec::<String>::new(), "{what}");
        }
        assert_eq!(
            missing_in(
                "class O extends Box { class I extends String { void n() { super.get(); } } }",
                &world()
            ),
            vec!["get"],
            "a plain super is the enclosing class's"
        );
    }

    /// A workspace member's type is written in another file and resolved
    /// there: certain only where the world says nothing there could have
    /// shadowed a name in it.
    #[test]
    fn a_chain_through_a_workspace_member_is_judged_where_its_type_is_certain() {
        let sited = |mut member: ExternalMember, certain: bool| {
            member.site = Some(crate::externals::ExternalSite {
                doc_span: None,
                source: "/w/a/Near.java".to_owned(),
                name_start: 0,
                name_end: 3,
                name_range: Some(javelin_document::Range::new(
                    javelin_document::Position::new(0, 0),
                    javelin_document::Position::new(0, 3),
                )),
            });
            member.certain = certain;
            member
        };
        let world = world().with(
            "a.Near",
            &[],
            vec![
                sited(method("a.Near", "get", class(STRING), vec![]), false),
                sited(method("a.Near", "sure", class(BOX), vec![]), true),
            ],
        );
        assert_eq!(
            missing_in(
                "class A { void m(Near n, Box b) { n.get().nope(); b.get().nope(); } }",
                &world
            ),
            vec!["nope"],
            "only the chain through the JDK-shaped member"
        );
        let source = "class A { void m(Near n) { n.sure().nope(); } }";
        assert_eq!(missing_in(source, &world), vec!["nope"]);
        let written = written_in(source, &world);
        assert!(written.contains(&"Box".to_owned()), "{written:?}");
    }

    #[test]
    fn a_method_reference_is_never_judged() {
        assert_eq!(missing("Runnable r = s::nope;"), Vec::<String>::new());
    }

    /// `receiver.name` on a certain receiver: any member of the name, a
    /// method included, silences it, and so does a list that may be short.
    #[test]
    fn a_field_the_whole_receiver_lacks_is_reported() {
        assert_eq!(missing("int n = s.nope;"), vec!["nope"]);
        assert_eq!(missing("int n = b.get().nope;"), vec!["nope"]);
        assert_eq!(
            missing("Object o = Box.nope;"),
            vec!["nope"],
            "through a type"
        );
        for silent in [
            "int n = s.hash;",
            "int n = s.length;",
            "Object o = b.label;",
            "Object o = Box.make;",
            "int n = p.nope;",
            "String[] a = null; int n = a.length;",
            "String[] a = null; int n = a.nope;",
            "int i = 0; int n = i.nope;",
            "Object o = java.util.Nope.x;",
            "int n = s.cafe\u{301};",
        ] {
            assert_eq!(missing(silent), Vec::<String>::new(), "{silent}");
        }
    }

    /// `Box f()[]` returns a `Box[]`, which has a `length`.
    #[test]
    fn a_c_style_array_return_type_is_an_array() {
        assert_eq!(
            missing_in(
                "class A extends Box { Box f()[] { return null; } \
                 void m() { Object o = this.f().length; } }",
                &world()
            ),
            Vec::<String>::new()
        );
    }

    /// `Type.name` may be a nested type, which an external list does not
    /// hold: the caller is told, and one this file declares is not judged.
    #[test]
    fn a_field_through_a_type_name_says_so() {
        let source = "class A extends Box { String s; void m() { \
                      Object o = Box.nope; int n = s.nope; Object x = A.nope; } }";
        let document = TextDocument::new(DocumentId::new(FILE), 1, source);
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let scopes = ScopeTree::build(&document, &tree);
        let files = Files::single(FileId::new(FILE), &syntax, &scopes);
        let world = world();
        let at = |text: &str| {
            missing_member_at(
                &document,
                &tree,
                &files,
                &world,
                source.find(text).expect(text) as u32 + text.len() as u32 - 4,
            )
        };
        assert_eq!(
            at("Box.nope").expect("reported").used,
            MemberUse::Field { through_type: true }
        );
        assert_eq!(
            at("s.nope").expect("reported").used,
            MemberUse::Field {
                through_type: false
            }
        );
        assert_eq!(
            at("A.nope"),
            None,
            "`Box`'s nested types are not in `A`'s walk, and the caller asks no closure of `A`"
        );
    }

    /// An unqualified call no type around it declares or inherits.
    #[test]
    fn an_unqualified_call_nothing_around_it_has_is_reported() {
        let judged = |source: &str| missing_in(source, &world());
        assert_eq!(
            judged("class A extends Box { void m() { nope(); get(); length(); } }"),
            vec!["nope", "length"]
        );
        assert_eq!(
            judged(
                "class A extends Box { void outer() {} \
                 class I extends String { void m() { outer(); get(); length(); nope(); } } }"
            ),
            vec!["nope"],
            "each type around it is searched, its closure included"
        );
        for (source, why) in [
            (
                "class A extends Partial { void m() { nope(); } }",
                "a closure that may be short",
            ),
            (
                "class A extends Box { class I extends Partial { void m() { nope(); } } }",
                "an inner closure that may be short",
            ),
            (
                "class A { void m() { toString(); } }",
                "a name `Object` declares",
            ),
            (
                "class A extends Box { void m() { new Box() { void x() { nope(); } }; } }",
                "an anonymous class",
            ),
            ("enum E { X; void m() { nope(); } }", "an enum"),
            ("record R() { void m() { nope(); } }", "a record"),
            (
                "class A { int nope; void m() { nope(); } }",
                "a member of the name of any kind",
            ),
        ] {
            assert_eq!(judged(source), Vec::<String>::new(), "{why}");
        }
    }

    /// The supertypes the types around an unqualified call write are
    /// resolved against this file, so each is a name inheritance could
    /// shadow.
    #[test]
    fn an_unqualified_call_reports_what_the_types_around_it_wrote() {
        let source = "class A extends Box { class I extends L { void m() { nope(); } } } \
                      class L extends Box {}";
        let document = TextDocument::new(DocumentId::new(FILE), 1, source);
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let scopes = ScopeTree::build(&document, &tree);
        let files = Files::single(FileId::new(FILE), &syntax, &scopes);
        let found = missing_member_at(
            &document,
            &tree,
            &files,
            &world(),
            source.find("nope").expect("call") as u32,
        )
        .expect("reported");
        assert_eq!(found.used, MemberUse::Unqualified);
        assert_eq!(found.receiver, "I");
        assert_eq!(found.written, vec!["L", "Box", "Box"]);
    }

    #[test]
    fn the_chain_reports_what_it_resolved() {
        let source = "class A { Box b; void m() { b.get().nope(); Box.nope(); } }";
        let document = TextDocument::new(DocumentId::new(FILE), 1, source);
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let scopes = ScopeTree::build(&document, &tree);
        let files = Files::single(FileId::new(FILE), &syntax, &scopes);
        let world = world();
        let at = |text: &str| {
            missing_member_at(
                &document,
                &tree,
                &files,
                &world,
                source.find(text).expect(text) as u32,
            )
            .expect(text)
        };
        let chained = at("nope(); Box");
        assert_eq!(chained.receiver, STRING);
        assert_eq!(
            chained.written,
            vec!["Box", "String"],
            "a JDK hop's type as resolved, which a workspace edge may have bound"
        );
        assert_eq!(chained.type_name, None);
        let static_call = at("nope(); }");
        assert_eq!(static_call.receiver, BOX);
        assert_eq!(static_call.type_name.as_deref(), Some("Box"));
    }

    /// `java.util.List<E>` beside [`world`]'s types, whose `get` returns the
    /// type's own variable.
    fn generic_world() -> World {
        world()
            .with(
                "java.util.List",
                &[],
                vec![method(
                    "java.util.List",
                    "get",
                    ExternalType::Variable("E".into()),
                    vec![int()],
                )],
            )
            .generic(&["E"])
    }

    #[test]
    fn a_chain_through_a_type_argument_is_certain_and_an_unbound_one_is_silent() {
        let judged = |body: &str| {
            missing_in(
                &format!("class A {{ void m() {{ {body} }} }}"),
                &generic_world(),
            )
        };
        assert_eq!(
            judged("List<String> l = null; l.get(0).nope();"),
            vec!["nope"]
        );
        assert_eq!(
            judged("var l = (List<String>) null; l.get(0).nope();"),
            vec!["nope"]
        );
        for silent in [
            "List l = null; l.get(0).nope();",
            "List<?> l = null; l.get(0).nope();",
            "List<? extends String> l = null; l.get(0).nope();",
            "List<String> l = null; l.get(0).length();",
        ] {
            assert_eq!(judged(silent), Vec::<String>::new(), "{silent}");
        }
    }

    /// `type_of` types `x` in `l.forEach(x -> …)` now; this judge still does
    /// not, because overload resolution with a lambda argument is where javac
    /// and this engine are likeliest to part.
    #[test]
    fn a_lambda_parameter_is_never_a_certain_receiver() {
        let consumer = "java.util.function.Consumer";
        let mut accept = method(
            consumer,
            "accept",
            ExternalType::Void,
            vec![ExternalType::Variable("T".into())],
        );
        accept.is_abstract = true;
        let world = generic_world()
            .with(
                "java.util.Iterable",
                &[],
                vec![method(
                    "java.util.Iterable",
                    "forEach",
                    ExternalType::Void,
                    vec![ExternalType::Parameterized(
                        consumer.into(),
                        vec![ExternalType::Super(Box::new(ExternalType::Variable(
                            "E".into(),
                        )))],
                    )],
                )],
            )
            .generic(&["E"])
            .with(consumer, &[], vec![accept])
            .generic(&["T"]);
        assert_eq!(
            missing_in(
                "class A { void m(Iterable<String> l) { l.forEach(x -> x.nope()); } }",
                &world
            ),
            Vec::<String>::new()
        );
    }

    /// What a generic method's call binds its own type variable to is an
    /// inference, and a receiver typed by one is never certain: `of(s)`'s
    /// `T` is `String` to navigation and nothing to this.
    #[test]
    fn a_receiver_a_generic_methods_inference_types_is_never_certain() {
        let optional = "java.util.Optional";
        let mut of = method(
            optional,
            "of",
            ExternalType::Unresolved("T".into()),
            vec![ExternalType::Unresolved("T".into())],
        );
        of.is_static = true;
        of.type_parameters = vec!["T".into()];
        let world = world().with(optional, &[], vec![of]).generic(&["T"]);
        assert_eq!(
            missing_in(
                "import java.util.Optional; class A { void m(String s) { Optional.of(s).nope(); } }",
                &world
            ),
            Vec::<String>::new()
        );
    }

    /// A type argument can become the receiver, so its name is one
    /// inheritance could shadow as much as the base's is.
    #[test]
    fn the_chain_reports_the_type_arguments_a_receiver_was_written_with() {
        let source = "class A { List<Box> l; void m() { l.get(0).nope(); } }";
        let document = TextDocument::new(DocumentId::new(FILE), 1, source);
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let scopes = ScopeTree::build(&document, &tree);
        let files = Files::single(FileId::new(FILE), &syntax, &scopes);
        let found = missing_member_at(
            &document,
            &tree,
            &files,
            &generic_world(),
            source.find("nope").expect("call") as u32,
        )
        .expect("reported");
        assert_eq!(found.receiver, BOX);
        assert_eq!(found.written, vec!["List", "Box", "Box"]);
    }

    /// `Outer<String>.Inner` is a member type of `Outer`, whose own members
    /// say nothing about it.
    #[test]
    fn a_member_type_of_a_parameterized_type_is_silent() {
        let world = world().with("a.Outer", &[], Vec::new());
        assert_eq!(
            missing_in(
                "class A { Outer<String>.Inner x; void m(Outer<String>.Inner<Box> y) { \
                 x.inner(); y.inner(); } }",
                &world
            ),
            Vec::<String>::new()
        );
    }

    /// A file-local type's supertypes are resolved against this file too.
    #[test]
    fn the_chain_reports_the_supertypes_a_file_local_receiver_writes() {
        let source =
            "class A { L l; void m() { l.nope(); } } class L extends K {} class K extends Box {}";
        let document = TextDocument::new(DocumentId::new(FILE), 1, source);
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let scopes = ScopeTree::build(&document, &tree);
        let files = Files::single(FileId::new(FILE), &syntax, &scopes);
        let found = missing_member_at(
            &document,
            &tree,
            &files,
            &world(),
            source.find("nope").expect("call") as u32,
        )
        .expect("reported");
        assert_eq!(found.written, vec!["L", "K", "Box"]);
    }
}
