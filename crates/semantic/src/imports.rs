//! What a static import brings into scope (JLS 7.5.3, 7.5.4), where nothing
//! closer means the name (JLS 6.4.1, 15.12.1).
//!
//! An enclosing type's member of the name shadows any import of it, so the
//! import is reached only once every enclosing type is proven not to have
//! one: each walk whole but for `Object`, in both halves, and no type the
//! search cannot see past. Anything less answers what the name answered
//! before, never the import.

use javelin_parser::{Decl, DeclKind, FileSyntax, ImportDecl};

use crate::infer::{can_see_past, declared, enclosing_types, writes_supertype};
use crate::members::walk_members;
use crate::{Externals, Files, Member, Type};

/// Which of Java's namespaces a name is looked up in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Namespace {
    /// An unqualified call's name.
    Method,
    /// A bare name standing for a value.
    Value,
}

impl Namespace {
    fn holds(self, kind: DeclKind) -> bool {
        match self {
            Namespace::Method => kind == DeclKind::Method,
            Namespace::Value => matches!(
                kind,
                DeclKind::Field | DeclKind::EnumConstant | DeclKind::RecordComponent
            ),
        }
    }
}

/// Whether `member` is one a static import can bring in: an instance member
/// of the imported type never is.
pub(crate) fn importable(member: &Member<'_>) -> bool {
    member.external().is_some_and(|facts| facts.is_static)
}

/// The type a static import names for `name` at `offset`, where no
/// enclosing type can mean it.
///
/// A single-static import of the name shadows every on-demand one, and all
/// of them must name one type. Failing one, exactly one on-demand import's type may hold it, and each such
/// type's static list must be closed for the others' silence to count.
pub(crate) fn imported_owner(
    files: &Files<'_>,
    externals: &dyn Externals,
    name: &str,
    namespace: Namespace,
    offset: u32,
) -> Option<Type> {
    let syntax = files.current_syntax();
    let mut single: Vec<&str> = Vec::new();
    let mut on_demand: Vec<&str> = Vec::new();
    for import in syntax.imports.iter().filter(|import| import.is_static) {
        if import.is_on_demand {
            on_demand.push(&import.name);
        } else if let Some((owner, last)) = import.name.rsplit_once('.')
            && last == name
        {
            single.push(owner);
        }
    }
    on_demand.sort_unstable();
    on_demand.dedup();
    if (single.is_empty() && on_demand.is_empty())
        || may_rewrite(syntax)
        || !nothing_closer(files, externals, name, namespace, offset)
    {
        return None;
    }
    // What the single import's type does not hold is nothing: Java's
    // answer is then no member, or a nested type the tiers cannot read.
    if let [owner, rest @ ..] = single.as_slice() {
        if rest.iter().any(|other| other != owner) {
            return None;
        }
        // A single import hides an on-demand method only of its own
        // signature (JLS 6.4.1), so another of the name competes with it.
        if namespace == Namespace::Method
            && on_demand
                .iter()
                .any(|other| holder(other, files, externals, name, namespace) != Some(None))
        {
            return None;
        }
        return owner_type(owner, externals);
    }
    let mut found = None;
    for owner in on_demand {
        match holder(owner, files, externals, name, namespace)? {
            Some(_) if found.is_some() => return None,
            Some(ty) => found = Some(ty),
            None => {}
        }
    }
    found
}

/// Whether the type an on-demand import names holds `name`: `None` where
/// its static list is not closed, so its silence proves nothing.
fn holder(
    owner: &str,
    files: &Files<'_>,
    externals: &dyn Externals,
    name: &str,
    namespace: Namespace,
) -> Option<Option<Type>> {
    let ty = owner_type(owner, externals)?;
    let walk = walk_members(&ty, files, externals, true);
    let has = walk
        .members
        .iter()
        .any(|member| member.name() == name && namespace.holds(member.kind()));
    walk.closed.then_some(has.then_some(ty))
}

/// A type an import names, canonically, where the world or the index holds
/// it.
fn owner_type(fqn: &str, externals: &dyn Externals) -> Option<Type> {
    externals.members(fqn)?;
    Some(Type::External {
        name: fqn.rsplit('.').next().unwrap_or(fqn).to_owned(),
        fqn: fqn.to_owned(),
        arguments: Vec::new(),
    })
}

/// Whether every type enclosing `offset` is proven to have no member of
/// `name` in `namespace`.
fn nothing_closer(
    files: &Files<'_>,
    externals: &dyn Externals,
    name: &str,
    namespace: Namespace,
    offset: u32,
) -> bool {
    let chain = enclosing_types(files, offset);
    let imports = &files.current_syntax().imports;
    let generates = |decl: &Decl| may_generate(decl, imports, name, namespace);
    !chain.is_empty()
        && chain.iter().enumerate().all(|(level, decl)| {
            let shadowable = writes_supertype(decl)
                && chain[level + 1..]
                    .iter()
                    .any(|outer| writes_supertype(outer));
            let ty = declared(files, decl);
            can_see_past(decl.kind)
                && !shadowable
                && !generates(decl)
                && [false, true].into_iter().all(|statics| {
                    let walk = walk_members(&ty, files, externals, statics);
                    walk.whole_but_object
                        && walk.members.iter().all(|member| {
                            !member.local_decl().is_some_and(generates)
                                && !member.local_owner().is_some_and(generates)
                        })
                        && !walk
                            .members
                            .iter()
                            .any(|member| member.name() == name && namespace.holds(member.kind()))
                })
        })
}

/// Whether an annotation on `decl` may make Lombok add a member called `name`
/// in `namespace` to the type the walk did not see — the one generator that
/// writes into a class rather than beside it, which `may_rewrite` does not
/// already refuse. Another annotation adds nothing: a processor writes new
/// files. A constructor annotation adds a method only under `staticName`, a
/// log annotation a field, `@FieldNameConstants` constants, and the rest
/// methods.
///
/// A log field `lombok.config` renames is seen once the index holds the
/// type's file, as a generated member of the walk below.
///
/// ponytail: before that, one renamed to anything but `log` or `logger`, in
/// any case, is not seen. The upgrade is the configured name passed through
/// `Externals`.
pub(crate) fn may_generate(
    decl: &Decl,
    imports: &[ImportDecl],
    name: &str,
    namespace: Namespace,
) -> bool {
    lombok_annotations(decl, imports).any(|(simple, arguments)| match simple {
        "NoArgsConstructor" | "RequiredArgsConstructor" | "AllArgsConstructor" => {
            namespace == Namespace::Method && arguments.contains("staticName")
        }
        _ if LOGS.contains(&simple) => {
            namespace == Namespace::Value
                && matches!(name.to_ascii_lowercase().as_str(), "log" | "logger")
        }
        "FieldNameConstants" => namespace == Namespace::Value,
        _ => namespace == Namespace::Method,
    })
}

/// Whether `decl` carries a Lombok log annotation, which writes a field.
pub(crate) fn logs(decl: &Decl, imports: &[ImportDecl]) -> bool {
    lombok_annotations(decl, imports).any(|(simple, _)| LOGS.contains(&simple))
}

const LOGS: [&str; 9] = [
    "Slf4j",
    "XSlf4j",
    "Log",
    "Log4j",
    "Log4j2",
    "CommonsLog",
    "JBossLog",
    "Flogger",
    "CustomLog",
];

/// Each annotation on `decl` that may be Lombok's: its simple name and the
/// text after its `(`.
fn lombok_annotations<'d>(
    decl: &'d Decl,
    imports: &'d [ImportDecl],
) -> impl Iterator<Item = (&'d str, &'d str)> {
    decl.modifiers
        .iter()
        .filter_map(|modifier| modifier.strip_prefix('@'))
        .filter_map(|annotation| {
            let (written, arguments) = annotation
                .split_once('(')
                .map_or((annotation.trim(), ""), |(written, arguments)| {
                    (written.trim(), arguments)
                });
            Some((lombok_name(written, imports)?, arguments))
        })
}

/// The simple name of an annotation written as `written`, where it may be
/// Lombok's: written under `lombok.`, or imported from there by that name or
/// on demand.
fn lombok_name<'a>(written: &'a str, imports: &[ImportDecl]) -> Option<&'a str> {
    if written.starts_with("lombok.") {
        return written.rsplit('.').next();
    }
    let imported = !written.contains('.')
        && imports.iter().any(|import| {
            (import.name == "lombok" || import.name.starts_with("lombok."))
                && (import.is_on_demand || import.name.rsplit('.').next() == Some(written))
        });
    imported.then_some(written)
}

/// Whether the file can name a Lombok annotation that adds a member to a
/// type unseen — `@Delegate`, `@ExtensionMethod` and the rest of
/// `lombok.experimental`.
///
/// ponytail: one written fully qualified, `@lombok.experimental.Delegate`
/// with no import, is not seen here, and only `may_generate` sees it on the
/// enclosing types' own members. The upgrade is reading every declaration's
/// modifiers, which is file-sized, so it belongs in `FileSyntax` built once
/// and not on this request-path check.
fn may_rewrite(syntax: &FileSyntax) -> bool {
    syntax.imports.iter().any(|import| {
        import.name.starts_with("lombok.experimental")
            || import.name == "lombok.Delegate"
            || (import.is_on_demand && import.name == "lombok")
    })
}
