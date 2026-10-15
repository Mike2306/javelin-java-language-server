//! What an expression evaluates to, as far as the files a query can reach and
//! the world it can ask will say.
//!
//! The set of forms handled is fixed by [`javelin_parser::Expr`]: an expression
//! the parser did not project becomes `Expr::Opaque`, and an `Opaque` is
//! unknown. Nothing here has to decide where the list ends.

use javelin_parser::{BindingKind, Decl, DeclKind, Expr, LambdaParameter, Literal, Span};

use crate::apply::{excluded_by_arguments, hides_the_rest, most_specific};
use crate::externals::{ExternalType, Externals};
use crate::files::{DeclSite, Files};
use crate::imports::Namespace;
use crate::members::{Member, members_of};
use crate::resolve::resolve_written;
use crate::types::{Primitive, Type};

/// How deep a receiver chain `type_of` will follow.
///
/// The parser caps the chains it builds, but `Expr` is a public enum and Rust
/// gives variant fields the enum's visibility, so a caller can construct one
/// directly. Unlike `TypeRef`, this type cannot be sealed: handing an `Expr`
/// across the crate boundary is what the projection is for.
///
/// It now also bounds a chain the parser never built: every `var` hop spends a
/// level, so a long chain of `var` locals stops here, and a `var` cycle
/// (`var a = b; var b = a;`) terminates here rather than at the parser's
/// `MAX_CHAIN`, which only ever saw two one-link initialisers.
const MAX_RECEIVER_DEPTH: u32 = 64;

/// How many arguments one question may type, across the whole recursion.
///
/// The depth cap alone does not bound this work, because typing an argument
/// re-enters [`sole_candidate`], which types *its* arguments: a chain of
/// `var v = f(u, u);` doubles the work at every hop, so a linear amount of
/// source costs 2^depth typings. This bounds the total instead of the shape,
/// and — like the depth cap — it answers unknown rather than hanging.
const MAX_TYPED_ARGUMENTS: u32 = 256;

/// What one question may still spend, and — for a claim nobody asked for —
/// what it was read through.
pub(crate) struct Work {
    left: u32,
    trace: Option<Trace>,
}

/// A hint's strictness: every member picked on the way vouched for, and the
/// names it resolved kept for a caller that knows which ones may be shadowed.
#[derive(Default)]
struct Trace {
    doubtful: bool,
    written: Vec<String>,
}

impl Trace {
    fn record(&mut self, written: &str) {
        crate::missing::record(written, &mut self.written);
        // A primitive is a keyword, no name a tier reads or misreads.
        self.written.retain(|name| {
            !matches!(
                crate::types::parse_type_ref(name),
                crate::types::TypeRef::Primitive(_) | crate::types::TypeRef::Void
            )
        });
    }
}

impl Work {
    pub(crate) fn new(strict: bool) -> Self {
        Work {
            left: MAX_TYPED_ARGUMENTS,
            trace: strict.then(Trace::default),
        }
    }

    /// Record the names `written`, a type as written in the current file at
    /// `at`, spells — and doubt it inside a local or an anonymous class,
    /// where a member type its supertypes declare may mean another.
    fn wrote(&mut self, written: &str, at: u32, files: &Files<'_>) {
        if let Some(trace) = &mut self.trace {
            trace.record(written);
            if files.current_syntax().in_local_or_anonymous_class(at) {
                trace.doubtful = true;
            }
        }
    }

    /// Record every simple name a resolved type spells.
    fn resolved(&mut self, ty: &Type) {
        if let Some(trace) = &mut self.trace {
            crate::missing::record_resolved(ty, &mut trace.written);
        }
    }

    /// Whether `member`, picked from `owner`, can be shown unasked: picked
    /// from a list shown whole, not one of `Object`'s, and — for a workspace
    /// member — with a type its own file settles. Records the names its type
    /// was read through. A no-op outside a hint.
    fn vouch(
        &mut self,
        owner: &Type,
        member: &Member<'_>,
        files: &Files<'_>,
        externals: &dyn Externals,
    ) {
        let Some(trace) = &mut self.trace else {
            return;
        };
        let whole = whole(owner, files, externals);
        let workspace = member
            .external()
            .and_then(|facts| facts.site.as_ref())
            .is_some_and(|site| site.name_range.is_some());
        if !whole
            || crate::queries::OBJECT_METHODS.contains(&member.name())
            || (workspace && !member.external().is_some_and(|facts| facts.certain))
        {
            trace.doubtful = true;
            return;
        }
        // A member this file declares was read, and its overloads told apart,
        // through the types it writes: its result's and its parameters'.
        match member.declared_type() {
            Some(written) => {
                if member.local_decl().is_some_and(|decl| {
                    files
                        .current_syntax()
                        .in_local_or_anonymous_class(decl.span.start_byte)
                }) {
                    trace.doubtful = true;
                    return;
                }
                trace.record(written);
                for parameter in member.written_parameters() {
                    trace.record(parameter);
                }
            }
            None => crate::missing::record_resolved(
                &crate::members::member_type(member, files, externals),
                &mut trace.written,
            ),
        }
    }

    /// [`Work::vouch`] for a constructor, whose list is whole by
    /// construction: its overloads were told apart through the parameter
    /// types it writes, and a workspace one's are read where its own file
    /// settles them.
    fn vouch_constructor(&mut self, member: &Member<'_>, files: &Files<'_>) {
        let Some(trace) = &mut self.trace else {
            return;
        };
        // `resolve_written` asks the current file and then the world, so a
        // declaration it answers is this file's.
        if let Some(decl) = member.local_decl() {
            if files
                .current_syntax()
                .in_local_or_anonymous_class(decl.span.start_byte)
            {
                trace.doubtful = true;
                return;
            }
            for parameter in member.written_parameters() {
                trace.record(parameter);
            }
        } else if member.external().is_none_or(|facts| {
            facts
                .site
                .as_ref()
                .is_some_and(|site| site.name_range.is_some())
                && !facts.certain
        }) {
            trace.doubtful = true;
        }
    }

    /// Doubt a call any of whose arguments is raw, or that cannot be typed
    /// within the budget: Java's unchecked conversion (JLS 15.12.2.6) erases
    /// the result, which the binding does not model.
    fn unchecked(
        &mut self,
        arguments: &[Expr],
        files: &Files<'_>,
        externals: &dyn Externals,
        depth: u32,
    ) {
        if !self.strict() {
            return;
        }
        for argument in arguments {
            let Some(left) = self.left.checked_sub(1) else {
                self.doubt();
                return;
            };
            self.left = left;
            if raw(
                &type_at_depth(argument, files, externals, depth + 1, self),
                files,
                externals,
            ) {
                self.doubt();
                return;
            }
        }
    }

    fn doubt(&mut self) {
        if let Some(trace) = &mut self.trace {
            trace.doubtful = true;
        }
    }

    fn strict(&self) -> bool {
        self.trace.is_some()
    }
}

/// Whether both halves of `ty`'s member walk are whole but for `Object`.
/// A generic type written with no arguments, or an array of one.
fn raw(ty: &Type, files: &Files<'_>, externals: &dyn Externals) -> bool {
    match ty {
        Type::Array(element) => raw(element, files, externals),
        Type::Declared {
            decl, arguments, ..
        } => {
            arguments.is_empty()
                && files
                    .viewing(&decl.file)
                    .and_then(|view| view.current_syntax().declaration_at(decl.span))
                    .is_some_and(|declared| declared.type_parameters.is_some())
        }
        Type::External { fqn, arguments, .. } => {
            arguments.is_empty()
                && externals
                    .members(fqn)
                    .is_some_and(|members| !members.type_parameters.is_empty())
        }
        _ => false,
    }
}

fn whole(ty: &Type, files: &Files<'_>, externals: &dyn Externals) -> bool {
    [false, true]
        .into_iter()
        .all(|statics| crate::members::walk_members(ty, files, externals, statics).whole_but_object)
}

/// A type an inlay hint may claim, as a reader names it, and the first
/// segment of every type name it was read through — for a caller that knows
/// which names a static import, an inherited member type or a generated
/// source may mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintedType {
    pub label: String,
    pub written: Vec<String>,
}

/// `expr`'s type, where it can be shown on a line nobody asked about.
///
/// [`type_of`]'s own reading, held to more than definition is: every member
/// picked on the way — a hop, an argument typed to separate overloads, the
/// call a lambda is passed to — was picked from a list shown whole in both
/// halves, none is one of `Object`'s, and a workspace member's type is one
/// its own file settles. The answer names no unknown, no type variable and
/// no raw generic type, which may be an erasure's guess.
pub fn hinted_type(
    expr: &Expr,
    files: &Files<'_>,
    externals: &dyn Externals,
) -> Option<HintedType> {
    let mut work = Work::new(true);
    let ty = type_at_depth(expr, files, externals, 0, &mut work);
    vouched(ty, work, files, externals)
}

/// An implicit lambda parameter's type, under [`hinted_type`]'s rules.
pub fn lambda_parameter_hint(
    lambda: &LambdaParameter,
    files: &Files<'_>,
    externals: &dyn Externals,
) -> Option<HintedType> {
    if lambda.declared.is_some() {
        return None;
    }
    let mut work = Work::new(true);
    let ty = lambda_parameter_type(lambda, files, externals, 0, &mut work);
    vouched(ty, work, files, externals)
}

/// The constructor the `new` `expr` is selects, under [`hinted_type`]'s
/// rules, and the names its pick was read through — for a caller that knows
/// which ones a static import, an inherited member type or a generated source
/// may mean.
pub(crate) fn constructor_selected<'a>(
    expr: &Expr,
    files: &Files<'a>,
    externals: &dyn Externals,
) -> Option<(Member<'a>, Vec<String>)> {
    let Expr::New {
        type_text,
        span,
        arguments,
        anonymous,
    } = expr
    else {
        return None;
    };
    // An anonymous class's declaration spans its whole `new`, though its name
    // is read outside its body: the byte before the `new` is where it is read.
    let at = if *anonymous {
        span.start_byte.checked_sub(1)?
    } else {
        span.start_byte
    };
    let mut work = Work::new(true);
    work.wrote(type_text, at, files);
    let ty = resolve_written(type_text, files, externals, at);
    let found = sole_constructor(&ty, arguments, files, externals, 0, &mut work)?;
    work.vouch_constructor(&found, files);
    let trace = work.trace?;
    (!trace.doubtful).then_some((found, trace.written))
}

/// [`constructor_selected`] for `this(…)`, `super(…)` and an enum constant,
/// whose `receiver` is the type constructed: the class around the call, its
/// written superclass, or the enum.
pub(crate) fn explicit_constructor_selected<'a>(
    receiver: &Expr,
    arguments: &[Expr],
    files: &Files<'a>,
    externals: &dyn Externals,
) -> Option<(Member<'a>, Vec<String>)> {
    let mut work = Work::new(true);
    let ty = type_at_depth(receiver, files, externals, 0, &mut work);
    let found = sole_constructor(&ty, arguments, files, externals, 0, &mut work)?;
    work.vouch_constructor(&found, files);
    let trace = work.trace?;
    (!trace.doubtful).then_some((found, trace.written))
}

/// The type whose declaration `textDocument/typeDefinition` jumps to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeTarget {
    /// A type a reachable file declares, at its name.
    Declared(DeclSite),
    /// A type no reachable file declares, for the caller to place.
    External(String),
}

/// What [`type_definition_at`] answers: the target, and the names its typing
/// read through the resolver, which the caller gates as a hint's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDefinition {
    pub target: TypeTarget,
    pub written: Vec<String>,
}

/// The declaration of the type the value at `offset` has, typed under
/// [`hinted_type`]'s rules — except that a raw type answers, since the
/// declaration it names is the same whatever its arguments.
///
/// The outermost declared type is the target, through any array: a
/// `List<Order>` is `List` and an `Order[]` is `Order`. A primitive, `void`,
/// `null`, a type variable and anything unknown answer `None`.
pub fn type_definition_at(
    document: &javelin_document::TextDocument,
    tree: &javelin_parser::SyntaxTree,
    files: &Files<'_>,
    externals: &dyn Externals,
    offset: u32,
) -> Option<TypeDefinition> {
    let expr = javelin_parser::value_at(document, tree, offset)?;
    let mut work = Work::new(true);
    let mut ty = type_at_depth(&expr, files, externals, 0, &mut work);
    let trace = work.trace?;
    if trace.doubtful {
        return None;
    }
    while let Type::Array(element) = ty {
        ty = *element;
    }
    let target = match ty {
        Type::Declared { decl, .. } => TypeTarget::Declared(decl),
        Type::External { fqn, .. } => TypeTarget::External(fqn),
        _ => return None,
    };
    Some(TypeDefinition {
        target,
        written: trace.written,
    })
}

fn vouched(
    ty: Type,
    work: Work,
    files: &Files<'_>,
    externals: &dyn Externals,
) -> Option<HintedType> {
    let trace = work.trace?;
    if trace.doubtful {
        return None;
    }
    Some(HintedType {
        label: hint_label(&ty, files, externals, 0)?,
        written: trace.written,
    })
}

/// How a hint names `ty`, or `None` for anything it cannot claim.
fn hint_label(
    ty: &Type,
    files: &Files<'_>,
    externals: &dyn Externals,
    depth: u32,
) -> Option<String> {
    if depth > MAX_RECEIVER_DEPTH {
        return None;
    }
    let generic = |simple: &str, declares: bool, arguments: &[Type]| -> Option<String> {
        // No arguments on a generic type is a raw one, which an erasure may
        // have produced where Java's answer has them.
        if arguments.is_empty() {
            return (!declares).then(|| simple.to_owned());
        }
        let inner = arguments
            .iter()
            .map(|argument| hint_label(argument, files, externals, depth + 1))
            .collect::<Option<Vec<_>>>()?;
        Some(format!("{simple}<{}>", inner.join(", ")))
    };
    match ty {
        Type::Primitive(primitive) => Some(primitive.keyword().to_owned()),
        Type::Array(element) => Some(format!(
            "{}[]",
            hint_label(element, files, externals, depth + 1)?
        )),
        Type::Declared {
            name,
            decl,
            arguments,
        } => {
            let view = files.viewing(&decl.file)?;
            let declared = view.current_syntax().declaration_at(decl.span)?;
            let simple = name.rsplit('.').next().unwrap_or(name);
            generic(simple, declared.type_parameters.is_some(), arguments)
        }
        Type::External { fqn, arguments, .. } => {
            let simple = fqn.rsplit('.').next().unwrap_or(fqn);
            let declares = externals.members(fqn)?.type_parameters.len();
            if !arguments.is_empty() && arguments.len() != declares {
                return None;
            }
            generic(simple, declares > 0, arguments)
        }
        Type::Void | Type::Null | Type::Variable(_) | Type::Unknown(_) => None,
    }
}

/// The type of an expression.
///
/// Unknown is still the ordinary answer, and what it covers has narrowed
/// twice. A name the classpath resolves types as [`Type::External`], and a
/// member can now be selected on one — so a chain does not end at the first
/// type the open file does not declare, and a string literal is a
/// `java.lang.String` wherever the world holds one.
pub fn type_of(expr: &Expr, files: &Files<'_>, externals: &dyn Externals) -> Type {
    let mut work = Work::new(false);
    type_at_depth(expr, files, externals, 0, &mut work)
}

fn type_at_depth(
    expr: &Expr,
    files: &Files<'_>,
    externals: &dyn Externals,
    depth: u32,
    work: &mut Work,
) -> Type {
    if depth > MAX_RECEIVER_DEPTH {
        return Type::Unknown(String::new());
    }
    let syntax = files.current_syntax();
    let scopes = files.current_scopes();
    match expr {
        Expr::Name { name, span } => {
            // A pattern variable is bound to the end of its scope, where Java
            // ends it by definite assignment, so a variable or a field it
            // seems to hide may be what the name means.
            if work.strict() && pattern_may_be_another(name, span, files, externals) {
                work.doubt();
            }
            // A name is a value first and a type second: `Box.` and `box.` are
            // both legal, and the variable namespace shadows nothing here.
            if let Some(binding) = scopes.resolve_value(span.start_byte, name)
                && let Some(decl) = syntax.declaration_at(binding.name_span)
                && let Some(written) = &decl.declared_type
            {
                // From a nested type, a field the nested type inherits would
                // shadow this one (JLS 6.4.1), which the scope tree does not
                // know: a hint does not claim one.
                if work.strict()
                    && matches!(decl.kind, DeclKind::Field | DeclKind::RecordComponent)
                    && enclosing_type(files, span.start_byte)
                        != enclosing_type(files, decl.name_span.start_byte)
                {
                    work.doubt();
                }
                if written != "var" {
                    work.wrote(written, decl.name_span.start_byte, files);
                }
                // Inside the method a variadic `String... args` is a `String[]`.
                if decl.varargs {
                    let array = format!("{written}[]");
                    return resolve_written(&array, files, externals, decl.name_span.start_byte);
                }
                if written != "var" {
                    return resolve_written(written, files, externals, decl.name_span.start_byte);
                }
                // `var` has no type of its own; the initialiser has it. The
                // recursion goes through `type_at_depth` with the running
                // depth rather than through `type_of`, because `var a = a;`
                // and `var a = b; var b = a;` resolve back to themselves —
                // only a depth that keeps climbing terminates them.
                return match decl.initializer.as_deref() {
                    Some(initializer) => {
                        type_at_depth(initializer, files, externals, depth + 1, work)
                    }
                    None => Type::Unknown("var".into()),
                };
            }
            // A lambda's parameter is bound and declared by no `Decl`.
            if let Some(binding) = scopes.resolve_value(span.start_byte, name)
                && binding.kind == BindingKind::Parameter
                && let Some(lambda) = syntax.lambda_parameter(binding.name_span.start_byte)
            {
                return lambda_parameter_type(lambda, files, externals, depth, work);
            }
            // A variable nothing above types — a record pattern's component —
            // still obscures a type of its name (JLS 6.4.2).
            if scopes.resolve_value(span.start_byte, name).is_some() {
                return Type::Unknown(name.clone());
            }
            // A value before a type (JLS 6.5.2): a generated field, and an
            // imported one only where nothing in scope is.
            if scopes.resolve_value(span.start_byte, name).is_none()
                && let Some((ty, member)) = field_owner(files, externals, name, span.start_byte)
            {
                // `member_type`'s pick, which for a value spends no work.
                work.vouch(&ty, &member, files, externals);
                return crate::members::member_type(&member, files, externals);
            }
            if scopes.resolve_value(span.start_byte, name).is_none()
                && let Some(imported) = crate::imports::imported_owner(
                    files,
                    externals,
                    name,
                    Namespace::Value,
                    span.start_byte,
                )
            {
                let owner = Owner::Imported(imported);
                return member_type(&owner, name, Access::Value, files, externals, depth, work);
            }
            // A field an enclosing type inherits outranks the type (JLS
            // 6.5.2), and the scope tree binds only the fields it declares.
            if work.strict() && field_may_shadow(files, externals, name, span.start_byte) {
                work.doubt();
            }
            work.wrote(name, span.start_byte, files);
            resolve_written(name, files, externals, span.start_byte)
        }
        Expr::This { span } => enclosing_type(files, span.start_byte),
        Expr::Super { span } => {
            let Type::Declared { decl, .. } = enclosing_type(files, span.start_byte) else {
                return Type::Unknown("super".into());
            };
            // Both halves in the file the site names rather than in the
            // current one: the `extends` clause is written there, so that
            // file's declaration tree holds it and that file's package,
            // imports and scopes are what it means against. The two are the
            // same file today — `enclosing_type` only ever answers with
            // `files.current()` — and going through the site rather than
            // around it is what keeps that from being something a reader has
            // to check another function to know.
            let Some(view) = files.viewing(&decl.file) else {
                return Type::Unknown("super".into());
            };
            let Some(enclosing) = view.current_syntax().declaration_at(decl.span) else {
                return Type::Unknown("super".into());
            };
            match &enclosing.extends {
                Some(written) => {
                    work.wrote(written, enclosing.name_span.start_byte, files);
                    resolve_written(written, &view, externals, enclosing.name_span.start_byte)
                }
                None => Type::Unknown("super".into()),
            }
        }
        Expr::FieldAccess { receiver, name, .. } => {
            let owner =
                Owner::Enclosing(type_at_depth(receiver, files, externals, depth + 1, work));
            member_type(&owner, name, Access::Value, files, externals, depth, work)
        }
        Expr::Call { .. } => call_type_toward(expr, None, files, externals, depth, work),
        Expr::New {
            type_text,
            span,
            anonymous,
            ..
        } => {
            // An anonymous class's own members — a covariant override among
            // them — are callable on it, and no list holds them.
            if *anonymous {
                work.doubt();
            }
            work.wrote(type_text, span.start_byte, files);
            resolve_written(type_text, files, externals, span.start_byte)
        }
        Expr::Cast {
            type_text,
            inner,
            span,
        } => {
            // The result comes from the written type alone, but `inner` is
            // still a receiver chain a caller could nest arbitrarily deep
            // (`((Box) ((Box) ((Box) ...)))`), so it still counts against the
            // depth cap even though its own type is never used.
            let _ = type_at_depth(inner, files, externals, depth + 1, work);
            work.wrote(type_text, span.start_byte, files);
            resolve_written(type_text, files, externals, span.start_byte)
        }
        Expr::ArrayAccess { array, .. } => {
            match type_at_depth(array, files, externals, depth + 1, work) {
                Type::Array(element) => *element,
                other => Type::Unknown(format!("{other:?}")),
            }
        }
        // A literal needs no lookup: its type is fixed by its own grammar node.
        Expr::Literal { literal, .. } => match literal {
            Literal::Int => Type::Primitive(Primitive::Int),
            Literal::Long => Type::Primitive(Primitive::Long),
            Literal::Float => Type::Primitive(Primitive::Float),
            Literal::Double => Type::Primitive(Primitive::Double),
            Literal::Char => Type::Primitive(Primitive::Char),
            Literal::Boolean => Type::Primitive(Primitive::Boolean),
            Literal::String => string_type(externals),
            Literal::Null => Type::Null,
        },
        Expr::ClassLiteral { type_text, span } => {
            work.wrote(type_text, span.start_byte, files);
            let element = resolve_written(type_text, files, externals, span.start_byte);
            class_type(element, externals)
        }
        // JLS 14.14.2: an array's component, or the `X` of the `Iterable<X>`
        // the type is.
        Expr::Element { iterable, .. } => {
            match type_at_depth(iterable, files, externals, depth + 1, work) {
                Type::Array(element) => *element,
                ty if ty.is_known() => {
                    if work.strict() && !whole(&ty, files, externals) {
                        work.doubt();
                    }
                    iterable_element(&ty, files, externals)
                }
                other => other,
            }
        }
        // A lambda has no type of its own; the parameter it is passed to
        // gives it one, and a call reads its body through `bind`.
        Expr::Lambda { .. } | Expr::MethodRef { .. } | Expr::Opaque { .. } => {
            Type::Unknown(String::new())
        }
    }
}

/// A lambda parameter's type: its written one, or the one the call the lambda
/// is an argument of gives it — through that call's sole candidate, so every
/// refusal definition makes is made here too.
///
/// ponytail: only a lambda that is a call's argument is typed; one assigned
/// to a typed local, returned or passed to a constructor is not, though its
/// target type is written down. The upgrade is reading that target here.
fn lambda_parameter_type(
    lambda: &LambdaParameter,
    files: &Files<'_>,
    externals: &dyn Externals,
    depth: u32,
    work: &mut Work,
) -> Type {
    if let Some(written) = &lambda.declared {
        if written == "var" {
            return Type::Unknown("var".into());
        }
        work.wrote(written, lambda.name_span.start_byte, files);
        return resolve_written(written, files, externals, lambda.name_span.start_byte);
    }
    let Some(Expr::Call {
        receiver,
        name,
        name_span,
        arguments,
        ..
    }) = lambda.call.as_deref()
    else {
        return Type::Unknown(String::new());
    };
    let owner = match receiver {
        Some(receiver) => {
            Owner::Enclosing(type_at_depth(receiver, files, externals, depth + 1, work))
        }
        None => call_owner(files, externals, name, name_span.start_byte),
    };
    let access = Access::Call { arguments };
    candidate_at_depth(owner.ty(), name, access, files, externals, depth + 1, work)
        .filter(|member| owner.admits(member))
        .and_then(|member| {
            work.vouch(owner.ty(), &member, files, externals);
            work.unchecked(arguments, files, externals, depth);
            if work.strict()
                && let Some(target) =
                    crate::members::lambda_target_at(&member, lambda.argument, files, externals)
            {
                work.resolved(&target);
            }
            crate::members::lambda_parameter(
                &member,
                lambda.argument,
                lambda.position,
                lambda.arity,
                work.strict(),
                files,
                externals,
            )
        })
        .unwrap_or_else(|| Type::Unknown(String::new()))
}

/// The innermost type declaration containing an offset, in the current file.
pub(crate) fn enclosing_type(files: &Files<'_>, offset: u32) -> Type {
    match enclosing_types(files, offset).first() {
        Some(decl) => declared(files, decl),
        None => Type::Unknown(String::new()),
    }
}

/// Every type declaration containing an offset, in the current file,
/// innermost first — none inside an enum constant's body, which is an
/// anonymous class the parser does not declare: its methods are no list's,
/// so neither the enum nor anything around it can say what a name there
/// means.
pub(crate) fn enclosing_types<'a>(files: &Files<'a>, offset: u32) -> Vec<&'a Decl> {
    fn visit<'a>(decls: &'a [Decl], offset: u32, ordered: bool, out: &mut Vec<&'a Decl>) -> bool {
        let Some(decl) = holding(decls, offset, ordered) else {
            return true;
        };
        if decl.kind == DeclKind::EnumConstant && !decl.children.is_empty() {
            return false;
        }
        if is_type(decl.kind) {
            out.push(decl);
        }
        visit(&decl.children, offset, ordered, out)
    }
    let mut out = Vec::new();
    let syntax = files.current_syntax();
    if !visit(
        &syntax.declarations,
        offset,
        syntax.in_source_order(),
        &mut out,
    ) {
        return Vec::new();
    }
    out.reverse();
    out
}

/// The last of `decls` holding `offset`, where two siblings meet at it;
/// bisected where the lists are in order, since a god-class's body is one
/// long list.
fn holding(decls: &[Decl], offset: u32, ordered: bool) -> Option<&Decl> {
    let holds = |decl: &&Decl| decl.span.start_byte <= offset && offset <= decl.span.end_byte;
    if ordered {
        decls[..decls.partition_point(|decl| decl.span.start_byte <= offset)]
            .last()
            .filter(holds)
    } else {
        decls.iter().rfind(holds)
    }
}

pub(crate) fn declared(files: &Files<'_>, decl: &Decl) -> Type {
    Type::Declared {
        name: decl.name.clone(),
        decl: DeclSite::new(files.current().clone(), decl.name_span),
        // ponytail: the enclosing type is used raw, so its own type
        // parameters stay Type::Variable rather than binding to anything.
        // Binding them needs a use site, which `this` does not have. The
        // upgrade is to bind them from the use that reached here.
        arguments: Vec::new(),
    }
}

/// The type an unqualified call to `name` at `offset` searches — JLS
/// 15.12.1: the innermost enclosing type of which a method of that name is a
/// member, whatever its arity, so a nested class reaches the class around it
/// and an inner `f(int)` hides an outer `f()`.
///
/// A `private` method another type declares is not a member (JLS 8.2), so
/// it does not stop the search.
///
/// The search stops at a type it cannot see past, and answers the innermost
/// one as if nothing around it were searched:
/// - a type whose list may be short — a supertype nothing resolved could
///   declare the method;
/// - an anonymous class and an annotation type, whose supertype no
///   declaration writes — an enum's or a record's walk is whole only where
///   the index holds what Java declares into it;
/// - a type that writes a supertype inside one that writes one too: the
///   outer one may inherit a member type of that name, which shadows what
///   the resolver read the name as (JLS 6.4.1), so the list walked may be
///   another type's;
/// - a type whose list holds a declaration annotated with anything but
///   `java.lang`'s own five — Lombok's `@Delegate`, or any annotation a
///   generator reads, may add the method to a list that looks whole.
///
/// A name `Object` declares is every class's, so the innermost has it.
///
/// Where no enclosing type has the method and the search saw every one, a
/// static import of the name is searched instead — see [`crate::imports`].
///
/// ponytail: an AspectJ inter-type declaration, written in another file and
/// annotated nowhere, is not seen. The upgrade is the build's `rewrites`
/// hint passed through `Externals`.
pub(crate) fn call_owner(
    files: &Files<'_>,
    externals: &dyn Externals,
    name: &str,
    offset: u32,
) -> Owner {
    let chain = enclosing_types(files, offset);
    let Some(innermost) = chain.first() else {
        return Owner::Enclosing(Type::Unknown(String::new()));
    };
    if crate::queries::OBJECT_METHODS.contains(&name) {
        return Owner::Enclosing(declared(files, innermost));
    }
    let otherwise = || match crate::imports::imported_owner(
        files,
        externals,
        name,
        Namespace::Method,
        offset,
    ) {
        Some(imported) => Owner::Imported(imported),
        None => Owner::Enclosing(declared(files, innermost)),
    };
    // A top-level type is the whole chain: nothing to search past.
    if chain.len() == 1 {
        return otherwise();
    }
    for (level, decl) in chain.iter().enumerate() {
        let ty = declared(files, decl);
        let mut whole = true;
        let mut plain = true;
        for statics in [false, true] {
            let walk = crate::members::walk_members(&ty, files, externals, statics);
            if walk.members.iter().any(|member| {
                member.kind() == DeclKind::Method
                    && member.name() == name
                    && !(member
                        .local_decl()
                        .is_some_and(|own| own.modifiers.iter().any(|m| m == "private"))
                        && !member.declared_by(&ty))
            }) {
                return Owner::Enclosing(ty);
            }
            whole &= walk.whole_but_object;
            plain &= walk.members.iter().all(|member| {
                member.local_decl().is_none_or(plain_annotations)
                    && member.local_owner().is_none_or(plain_annotations)
            });
        }
        let shadowable = writes_supertype(decl)
            && chain[level + 1..]
                .iter()
                .any(|outer| writes_supertype(outer));
        if !whole || !plain || shadowable || !can_see_past(decl.kind) {
            break;
        }
    }
    otherwise()
}

/// Whether a type around `offset` may hold a field `name`: one whose walk
/// is not whole, or holds a value member of that name.
fn field_may_shadow(files: &Files<'_>, externals: &dyn Externals, name: &str, offset: u32) -> bool {
    enclosing_types(files, offset).iter().any(|decl| {
        let frame = field_frame(files, externals, decl, &declared(files, decl));
        !frame.whole || frame.fields.contains(name)
    })
}

/// The enclosing type a bare `name` at `offset`, which no scope binds, means
/// a field of, by JLS 6.4.1 and 15.11: the innermost type with a field of the
/// name as a member — one it inherits, or one Lombok generates, `log`. The
/// scope tree binds only the fields a file declares, so a field a supertype
/// declares reaches here, and a local or a parameter of the name never does.
///
/// A `private` field another type declares is not a member (JLS 8.2), so it
/// does not stop the search. The search stops where [`call_owner`]'s does,
/// and also at a type a Lombok annotation may add the field to unseen
/// (`@CustomLog`).
///
/// ponytail: a field the scope tree binds in an outer type is not doubted
/// where a nested type between inherits one of the name, which Java means
/// instead; only strict typing doubts it. The upgrade is asking this search
/// before the scope tree's outer bindings.
pub(crate) fn field_owner<'a>(
    files: &Files<'a>,
    externals: &dyn Externals,
    name: &str,
    offset: u32,
) -> Option<(Type, Member<'a>)> {
    let mut chain = enclosing_types(files, offset);
    // A type's own annotations and header are outside its body, where
    // nothing it inherits is in scope (JLS 6.3); so are an anonymous class's
    // arguments.
    let ordered = files.current_syntax().in_source_order();
    if chain
        .first()
        .is_some_and(|innermost| holding(&innermost.children, offset, ordered).is_none())
    {
        chain.remove(0);
    }
    let imports = &files.current_syntax().imports;
    // Every bare type name reaches here: walk nothing that can neither
    // inherit a field nor be given a logger.
    if !chain
        .iter()
        .any(|decl| writes_supertype(decl) || crate::imports::logs(decl, imports))
    {
        return None;
    }
    for (level, decl) in chain.iter().enumerate() {
        let ty = declared(files, decl);
        let frame = field_frame(files, externals, decl, &ty);
        // The supertypes this type writes may name a member type one around
        // it inherits, so its list may be another type's — whether or not it
        // holds the field.
        let shadowable = writes_supertype(decl)
            && chain[level + 1..]
                .iter()
                .any(|outer| writes_supertype(outer));
        if shadowable {
            return None;
        }
        if frame.fields.contains(name) {
            let picked = frame
                .picked
                .borrow_mut()
                .entry(name.to_string())
                .or_insert_with(|| {
                    let found = sole_candidate(&ty, name, Access::Value, files, externals)?;
                    // A fact says `public` or not, so a protected field and a
                    // package-private one another package does not inherit
                    // (JLS 8.2) look alike.
                    let hidable = found.external().is_some_and(|member| !member.is_public)
                        && !found.declared_by(&ty);
                    let private = inherited_private(&found, &ty);
                    Some((found, private, hidable))
                })
                .clone();
            let (found, private, hidable) = picked?;
            // One that may not be inherited yields to anything else the name
            // may mean.
            let doubted = hidable
                && (level + 1 < chain.len()
                    || imports.iter().any(|import| {
                        import.is_static
                            && (import.is_on_demand || import.name.rsplit('.').next() == Some(name))
                    }));
            return (!private && !doubted).then_some((ty, found));
        }
        // A type Lombok writes a logger into whose walk holds none — a
        // record's, a `@CustomLog`'s, one not indexed yet — may hold it under
        // any configured name.
        if crate::imports::logs(decl, imports) && !frame.generated_field {
            return None;
        }
        if !frame.whole
            || !can_see_past(decl.kind)
            || crate::imports::may_generate(decl, imports, name, Namespace::Value)
        {
            return None;
        }
    }
    None
}

/// What [`field_owner`] reads off one enclosing type's walk, both halves.
#[derive(Debug)]
pub(crate) struct FieldFrame<'a> {
    /// Its value members' names, less another type's `private` field.
    fields: std::collections::HashSet<String>,
    whole: bool,
    /// Whether a generator wrote a field into it.
    generated_field: bool,
    /// Its value candidate of a name, by name, with whether it is a
    /// `private` field another type declares and whether it is a non-public
    /// fact it inherits. `None` where there is no one candidate.
    #[allow(clippy::type_complexity)]
    picked: std::cell::RefCell<std::collections::HashMap<String, Option<(Member<'a>, bool, bool)>>>,
}

/// `decl`'s [`FieldFrame`], walked once per [`Files`].
fn field_frame<'a>(
    files: &Files<'a>,
    externals: &dyn Externals,
    decl: &Decl,
    ty: &Type,
) -> std::rc::Rc<FieldFrame<'a>> {
    let key = (files.current().clone(), decl.name_span.start_byte);
    if let Some(frame) = files.fields.borrow().get(&key) {
        return std::rc::Rc::clone(frame);
    }
    let mut frame = FieldFrame {
        fields: Default::default(),
        whole: true,
        generated_field: false,
        picked: Default::default(),
    };
    for statics in [false, true] {
        let walk = crate::members::walk_members(ty, files, externals, statics);
        for member in &walk.members {
            if member.kind() == DeclKind::Field && member.external().is_some() {
                frame.generated_field |= member.declared_by(ty);
            }
            if matches!(
                member.kind(),
                DeclKind::Field | DeclKind::EnumConstant | DeclKind::RecordComponent
            ) && !inherited_private(member, ty)
            {
                frame.fields.insert(member.name().to_string());
            }
        }
        frame.whole &= walk.whole_but_object;
    }
    let frame = std::rc::Rc::new(frame);
    files
        .fields
        .borrow_mut()
        .insert(key, std::rc::Rc::clone(&frame));
    frame
}

/// What an unqualified call's name is searched in: an enclosing type, or
/// the type a static import names, of which only a static member is
/// imported.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Owner {
    Enclosing(Type),
    Imported(Type),
}

impl Owner {
    pub(crate) fn ty(&self) -> &Type {
        match self {
            Owner::Enclosing(ty) | Owner::Imported(ty) => ty,
        }
    }

    /// Whether a member picked off [`Owner::ty`] is one this search reaches.
    pub(crate) fn admits(&self, member: &Member<'_>) -> bool {
        match self {
            Owner::Enclosing(_) => true,
            Owner::Imported(_) => crate::imports::importable(member),
        }
    }
}

/// Whether `member` is a `private` one a type other than `owner` declares,
/// which `owner` does not inherit (JLS 8.2).
fn inherited_private(member: &Member<'_>, owner: &Type) -> bool {
    member
        .local_decl()
        .is_some_and(|own| own.modifiers.iter().any(|m| m == "private"))
        && !member.declared_by(owner)
}

pub(crate) fn writes_supertype(decl: &Decl) -> bool {
    decl.extends.is_some() || !decl.implements.is_empty()
}

/// Whether every annotation `decl` carries is one of `java.lang`'s own,
/// which no generator reads to add a member.
fn plain_annotations(decl: &Decl) -> bool {
    decl.modifiers
        .iter()
        .filter_map(|modifier| modifier.strip_prefix('@'))
        .all(|annotation| {
            let name = annotation.split('(').next().unwrap_or(annotation).trim();
            matches!(
                name.strip_prefix("java.lang.").unwrap_or(name),
                "Override"
                    | "Deprecated"
                    | "SuppressWarnings"
                    | "SafeVarargs"
                    | "FunctionalInterface"
            )
        })
}

/// Whether a member list the walk calls whole but for `Object` says a
/// declaration of `kind` has no other member: not for an anonymous class,
/// whose supertype the parser records as its name, nor for an annotation
/// type, whose `java.lang.annotation.Annotation` nobody writes. An enum's
/// and a record's walk says for itself whether the index covers what Java
/// declares into it.
pub(crate) fn can_see_past(kind: DeclKind) -> bool {
    !matches!(kind, DeclKind::AnonymousClass | DeclKind::AnnotationType)
}

fn is_type(kind: DeclKind) -> bool {
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

/// What is being selected, which decides which namespace and which arity can
/// answer it.
///
/// The argument slice's lifetime is independent of the `FileSyntax` a
/// `Member` borrows from: an `Access` is built from `Expr`s the caller already
/// owns (a `Selection` or an unqualified call), not from `syntax` itself.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Access<'a> {
    /// Java's variable namespace: a field, an enum constant, a nested type.
    Value,
    /// A method named without an argument list, as `b::label` does.
    MethodRef,
    /// A call: methods only, filtered by arity and then by argument type.
    Call { arguments: &'a [Expr] },
}

/// The type of the single member of `owner` called `name`.
///
/// Unknown when [`sole_candidate`] declines, which is the same refusal stated
/// as a type: an ambiguous access has no answer here.
fn member_type(
    owner: &Owner,
    name: &str,
    access: Access<'_>,
    files: &Files<'_>,
    externals: &dyn Externals,
    depth: u32,
    work: &mut Work,
) -> Type {
    member_type_toward(
        owner,
        name,
        access,
        None,
        (&[], 0),
        files,
        externals,
        depth,
        work,
    )
}

/// [`member_type`], with a generic call's open variables inferred toward the
/// parameter it is passed for, where it is one.
#[allow(clippy::too_many_arguments)]
fn member_type_toward(
    owner: &Owner,
    name: &str,
    access: Access<'_>,
    toward: Option<Toward<'_>>,
    (written, at): (&[String], u32),
    files: &Files<'_>,
    externals: &dyn Externals,
    depth: u32,
    work: &mut Work,
) -> Type {
    let Some(member) = candidate_at_depth(owner.ty(), name, access, files, externals, depth, work)
        .filter(|member| owner.admits(member))
    else {
        return Type::Unknown(name.to_owned());
    };
    work.vouch(owner.ty(), &member, files, externals);
    if let Access::Call { arguments } = access {
        work.unchecked(arguments, files, externals, depth);
    }
    // A generic method's own type variables are bound by what the call
    // passes, as far as that settles them and no further.
    if let Access::Call { arguments } = access
        && let Some(facts) = member.external()
        && !facts.type_parameters.is_empty()
    {
        // JLS 15.12.2.1: written type arguments are a generic method's own
        // variables, one for one, and nothing is inferred past them.
        if !written.is_empty() {
            if written.len() != facts.type_parameters.len() {
                return Type::Unknown(name.to_owned());
            }
            let bound = facts
                .type_parameters
                .iter()
                .zip(written)
                .filter_map(|(variable, text)| {
                    work.wrote(text, at, files);
                    let ty = resolve_written(text, files, externals, at);
                    // A primitive is no type argument: `List.<int>of` does not compile.
                    let reference = !matches!(ty, Type::Primitive(_) | Type::Void | Type::Null);
                    (ty.is_known() && reference).then(|| (variable.clone(), ty))
                })
                .collect();
            return crate::members::member_type_bound(&member, files, externals, &bound);
        }
        let Bindings { mut bound, open } =
            bind(&member, facts, arguments, files, externals, depth, work);
        if let Some(toward) = toward {
            toward.settle(&facts.declared, &open, &mut bound);
        }
        return crate::members::member_type_bound(&member, files, externals, &bound);
    }
    crate::members::member_type(&member, files, externals)
}

/// `expr`'s type, a call's inferred toward `toward` where it is given.
fn call_type_toward(
    expr: &Expr,
    toward: Option<Toward<'_>>,
    files: &Files<'_>,
    externals: &dyn Externals,
    depth: u32,
    work: &mut Work,
) -> Type {
    if depth > MAX_RECEIVER_DEPTH {
        return Type::Unknown(String::new());
    }
    match expr {
        Expr::Call {
            receiver: Some(receiver),
            name,
            name_span,
            arguments,
            type_arguments,
        } => {
            let owner =
                Owner::Enclosing(type_at_depth(receiver, files, externals, depth + 1, work));
            let access = Access::Call { arguments };
            member_type_toward(
                &owner,
                name,
                access,
                toward,
                (type_arguments, name_span.start_byte),
                files,
                externals,
                depth,
                work,
            )
        }
        Expr::Call {
            receiver: None,
            name,
            name_span,
            arguments,
            type_arguments,
        } => {
            let owner = call_owner(files, externals, name, name_span.start_byte);
            let access = Access::Call { arguments };
            member_type_toward(
                &owner,
                name,
                access,
                toward,
                (type_arguments, name_span.start_byte),
                files,
                externals,
                depth,
                work,
            )
        }
        _ => type_at_depth(expr, files, externals, depth, work),
    }
}

/// A string literal's type: `java.lang.String` whatever the file declares
/// (JLS 3.10.5), so a file-local `class String` does not take it over — and
/// unknown where the world has never heard of `java.lang.String`.
pub(crate) fn string_type(externals: &dyn Externals) -> Type {
    const STRING: &str = "java.lang.String";
    if externals.is_subtype(STRING, STRING) == Some(true) {
        Type::External {
            name: "String".into(),
            fqn: STRING.into(),
            arguments: Vec::new(),
        }
    } else {
        Type::Unknown("String".into())
    }
}

/// The `X` of the `Iterable<X>` `ty` is, read off `Iterable`'s own
/// `iterator()` as the member walk binds it — never an override's, whose
/// `Iterator` may be narrower than `X`, or generic where the type implements
/// `Iterable` raw and Java's element is `Object`. Unknown where the walk
/// reaches `Iterable` raw, or not at all.
fn iterable_element(ty: &Type, files: &Files<'_>, externals: &dyn Externals) -> Type {
    const ITERABLE: &str = "java.lang.Iterable";
    const ITERATOR: &str = "java.util.Iterator";
    let mut elements = crate::members::members_of(ty, files, externals, false)
        .into_iter()
        .filter(|member| {
            member.name() == "iterator"
                && member.external().is_some_and(|facts| {
                    facts.owner == ITERABLE && !facts.is_static && facts.parameters.is_empty()
                })
        })
        .map(
            |member| match crate::members::member_type(&member, files, externals) {
                Type::External {
                    fqn, mut arguments, ..
                } if fqn == ITERATOR && arguments.len() == 1 => arguments.remove(0),
                _ => Type::Unknown(String::new()),
            },
        );
    let Some(first) = elements.next() else {
        return Type::Unknown(String::new());
    };
    if elements.all(|other| other == first) {
        first
    } else {
        Type::Unknown(String::new())
    }
}

/// `T.class`'s type, `java.lang.Class<T>` (JLS 15.8.2), with a primitive or
/// `void` boxed — and unknown where the world holds no `java.lang.Class`.
fn class_type(element: Type, externals: &dyn Externals) -> Type {
    const CLASS: &str = "java.lang.Class";
    if externals.is_subtype(CLASS, CLASS) != Some(true) {
        return Type::Unknown("Class".into());
    }
    let boxed = match &element {
        Type::Primitive(primitive) => externals.boxed(*primitive),
        Type::Void => Some("java.lang.Void".to_owned()),
        _ => None,
    };
    let element = match boxed {
        Some(fqn) if externals.is_subtype(&fqn, &fqn) == Some(true) => Type::External {
            name: fqn.rsplit('.').next().unwrap_or(&fqn).to_owned(),
            fqn,
            arguments: Vec::new(),
        },
        Some(fqn) => Type::Unknown(fqn),
        None if matches!(element, Type::Primitive(_) | Type::Void) => Type::Unknown(String::new()),
        None => element,
    };
    Type::External {
        name: "Class".into(),
        fqn: CLASS.into(),
        arguments: vec![element],
    }
}

/// A generic method's own type variables, bound by unifying each parameter's
/// type with what the call passes for it (JLS 18, the part of it that needs
/// no bound and no target type): an argument of a known type binds a bare
/// variable to it, boxed if it is primitive; a parameterized argument binds
/// through the type's own arguments; and an implicit expression lambda passed
/// for a functional interface binds the variable its method returns to the
/// type of its body, when nothing it takes is that variable.
///
/// Equality, never a bound, and every argument has its say: a variable two
/// arguments bind differently — `List.of("a", 1)` — is unbound where Java
/// would infer their least upper bound, and so is one that any argument
/// mentions without binding it — an argument of unknown type, a wildcard, a
/// type that is not the parameter's, a lambda whose results differ — since
/// Java's answer is the bound of *all* of them. `null` constrains nothing. A
/// variable nothing binds is unknown. A missing answer either way; never a
/// guess.
///
/// A method reference binds what the interface returns as an expression
/// lambda's body does, from the referenced method's result — see
/// [`method_ref_result`]. What the interface takes it bounds from above, which
/// no argument settles and only a target's lower bound may — see
/// [`Toward::settle`] — so those variables are open rather than poisoned.
///
/// ponytail: no target type but the parameter a call is passed for, and no
/// subtype unification (an `ArrayList<String>` passed for `Collection<E>`
/// binds nothing). The upgrades are reading an assignment's or a return's
/// declared type as a target, and binding through the argument's supertype
/// closure with `members_of`'s substitution.
fn bind(
    call: &Member<'_>,
    facts: &crate::externals::ExternalMember,
    arguments: &[Expr],
    files: &Files<'_>,
    externals: &dyn Externals,
    depth: u32,
    work: &mut Work,
) -> Bindings {
    let own = &facts.type_parameters;
    let mut bound = std::collections::HashMap::new();
    let mut poisoned = std::collections::HashSet::new();
    // What a method reference takes: an upper bound (JLS 18.2.1), which a
    // target's lower bound still settles — and nothing else may.
    let mut upper = std::collections::HashSet::new();
    let last = facts.parameters.len().saturating_sub(1);
    // One argument for a variadic tail is read as the array it may be
    // (JLS 15.12.2.2's first two phases): a `String[]` there binds `E` to
    // `String`.
    let lone_tail = facts.variadic && arguments.len() == facts.parameters.len();
    for (index, argument) in arguments.iter().enumerate() {
        let Some(parameter) = facts.parameters.get(index.min(last)) else {
            break;
        };
        // Out of budget is out of arguments, and a binding the rest of them
        // had no say in is not one.
        let Some(left) = work.left.checked_sub(1) else {
            return Bindings::default();
        };
        work.left = left;
        let mut found = std::collections::HashMap::new();
        match argument {
            Expr::MethodRef { .. } => match lambda_target(parameter, own, externals) {
                Some(LambdaTarget {
                    inputs,
                    output,
                    arity,
                }) => {
                    upper.extend(inputs);
                    if let Some(target) = output {
                        let result = method_ref_result(
                            argument,
                            call,
                            index,
                            arity,
                            files,
                            externals,
                            depth + 1,
                            work,
                        );
                        unify(&target, &result, own, externals, &mut found);
                        poison_unbound(&target, own, &found, &mut poisoned);
                    }
                }
                None => poisoned.extend(mentioned(parameter, own).into_iter().cloned()),
            },
            Expr::Lambda { results, typed, .. } => match lambda_target(parameter, own, externals) {
                Some(LambdaTarget { inputs, output, .. }) => {
                    // What an implicit lambda takes is settled by the other
                    // arguments before its body is read (JLS 18.5.1); a typed
                    // one's written types are constraints of their own.
                    if *typed {
                        poisoned.extend(inputs);
                    }
                    if let Some(target) = output {
                        // A `null` result constrains nothing; any two others of
                        // different types have a least upper bound, not one type.
                        let mut result = None;
                        for value in results.iter().flatten() {
                            // Each result is a typing of its own.
                            let Some(left) = work.left.checked_sub(1) else {
                                result = None;
                                break;
                            };
                            work.left = left;
                            // A pattern variable a return reads may be a field
                            // Java means instead, so two returns that seem to
                            // agree may not.
                            if root_may_be_another(value, files, externals) {
                                result = None;
                                break;
                            }
                            match type_at_depth(value, files, externals, depth + 1, work) {
                                Type::Null => {}
                                ty if result.as_ref().is_none_or(|seen| *seen == ty) => {
                                    result = Some(ty);
                                }
                                _ => {
                                    result = None;
                                    break;
                                }
                            }
                        }
                        if let Some(result) = result {
                            unify(&target, &result, own, externals, &mut found);
                        }
                        poison_unbound(&target, own, &found, &mut poisoned);
                    }
                }
                None => poisoned.extend(mentioned(parameter, own).into_iter().cloned()),
            },
            _ => {
                let toward = (!(facts.variadic && index >= last)).then_some(Toward {
                    parameter,
                    subs: &call.subs,
                    own,
                });
                let given =
                    match call_type_toward(argument, toward, files, externals, depth + 1, work) {
                        Type::Null => continue,
                        Type::Array(element) if lone_tail && index == last => match *element {
                            element @ (Type::External { .. }
                            | Type::Declared { .. }
                            | Type::Array(_)) => element,
                            _ => {
                                poisoned.extend(mentioned(parameter, own).into_iter().cloned());
                                continue;
                            }
                        },
                        given => given,
                    };
                unify(parameter, &given, own, externals, &mut found);
                poison_unbound(parameter, own, &found, &mut poisoned);
            }
        }
        for (name, ty) in found {
            match bound.get(&name) {
                Some(seen) if *seen != ty => {
                    poisoned.insert(name);
                }
                _ => {
                    bound.insert(name, ty);
                }
            }
        }
    }
    let open = own
        .iter()
        .filter(|name| !poisoned.contains(*name) && !bound.contains_key(*name))
        .cloned()
        .collect();
    poisoned.extend(upper);
    bound.retain(|name, ty| !poisoned.contains(name) && ty.is_known());
    Bindings { bound, open }
}

/// What [`bind`] settled at one call: each variable it bound, and those it
/// left open — bound by nothing and constrained by nothing but what a method
/// reference takes — which only the parameter the call is passed for may
/// still settle.
#[derive(Default)]
struct Bindings {
    bound: std::collections::HashMap<String, Type>,
    open: std::collections::HashSet<String>,
}

/// The parameter a generic call is passed for, as JLS 18.5.2.1's target:
/// its type as the outer method declares it, the outer receiver's type
/// arguments for its owner's variables, and the outer method's own
/// variables, which say nothing here.
#[derive(Clone, Copy)]
struct Toward<'t> {
    parameter: &'t ExternalType,
    subs: &'t std::collections::HashMap<String, Type>,
    own: &'t [String],
}

impl Toward<'_> {
    /// Bind each of `open` the call's declared type writes where the target
    /// says one type: a type there by equality, `? super X` to `X`, the lower
    /// bound Java resolves it to (JLS 18.4). `? extends X`, an outer variable,
    /// anything not wholly known, or two positions that disagree, bind
    /// nothing. Both sides must name one type to be descended into.
    ///
    /// A variable the call's own arguments bound is unbound where the target
    /// says another type: that is a second lower bound, and Java's answer is
    /// the least upper bound of the two.
    fn settle(
        self,
        declared: &ExternalType,
        open: &std::collections::HashSet<String>,
        bound: &mut std::collections::HashMap<String, Type>,
    ) {
        type Found = std::collections::HashMap<String, Vec<Option<Type>>>;
        fn walk(
            toward: Toward<'_>,
            declared: &ExternalType,
            target: &ExternalType,
            settles: &dyn Fn(&str) -> bool,
            found: &mut Found,
        ) {
            match (declared, target) {
                (ExternalType::Unresolved(written), _) if settles(written.trim()) => {
                    let value = match target {
                        ExternalType::Super(lower) => toward.value(lower),
                        ExternalType::Extends(_) => None,
                        exact => toward.value(exact),
                    };
                    found
                        .entry(written.trim().to_owned())
                        .or_default()
                        .push(value);
                }
                (
                    ExternalType::Parameterized(fqn, arguments),
                    ExternalType::Parameterized(target_fqn, targets),
                ) if fqn == target_fqn && arguments.len() == targets.len() => {
                    for (declared, target) in arguments.iter().zip(targets) {
                        walk(toward, declared, target, settles, found);
                    }
                }
                _ => {}
            }
        }
        let mut found = Found::new();
        let settles = |name: &str| open.contains(name) || bound.contains_key(name);
        walk(self, declared, self.parameter, &settles, &mut found);
        for (name, values) in found {
            match bound.get(&name) {
                Some(binding) => {
                    if values.iter().flatten().any(|value| value != binding) {
                        bound.remove(&name);
                    }
                }
                None => {
                    if let [Some(first), rest @ ..] = values.as_slice()
                        && rest.iter().all(|value| value.as_ref() == Some(first))
                    {
                        bound.insert(name, first.clone());
                    }
                }
            }
        }
    }

    fn value(self, ty: &ExternalType) -> Option<Type> {
        if !mentioned(ty, self.own).is_empty() {
            return None;
        }
        let ty = crate::members::external_type(ty, self.subs);
        crate::apply::concrete(&ty).then_some(ty)
    }
}

/// Every one of `own` that `ty` mentions, anywhere in it — over-inclusive on
/// purpose: a `T[]` mentions `T` though nothing here unifies through it.
fn mentioned<'o>(ty: &ExternalType, own: &'o [String]) -> Vec<&'o String> {
    fn walk<'o>(ty: &ExternalType, own: &'o [String], out: &mut Vec<&'o String>) {
        match ty {
            ExternalType::Unresolved(written) => out.extend(own.iter().filter(|name| {
                written
                    .split(|character: char| {
                        !character.is_alphanumeric() && character != '_' && character != '$'
                    })
                    .any(|word| word == name.as_str())
            })),
            ExternalType::Parameterized(_, arguments) => {
                for argument in arguments {
                    walk(argument, own, out);
                }
            }
            ExternalType::Extends(inner) | ExternalType::Super(inner) => walk(inner, own, out),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(ty, own, &mut out);
    out
}

/// Whether the pattern variable `name` at `span` binds may instead mean a
/// variable or a field it seems to hide: the scope tree binds it to the end
/// of its scope, where Java ends it by definite assignment.
fn pattern_may_be_another(
    name: &str,
    span: &Span,
    files: &Files<'_>,
    externals: &dyn Externals,
) -> bool {
    let scopes = files.current_scopes();
    scopes
        .resolve_value(span.start_byte, name)
        .is_some_and(|binding| {
            binding.kind == BindingKind::PatternBinding
                && (scopes
                    .resolve_value(binding.name_span.start_byte.saturating_sub(1), name)
                    .is_some()
                    || field_may_shadow(files, externals, name, binding.name_span.start_byte))
        })
}

/// [`pattern_may_be_another`] for the name a chain starts at.
pub(crate) fn root_may_be_another(
    expr: &Expr,
    files: &Files<'_>,
    externals: &dyn Externals,
) -> bool {
    let mut expr = expr;
    loop {
        match expr {
            Expr::FieldAccess { receiver, .. }
            | Expr::Call {
                receiver: Some(receiver),
                ..
            } => expr = receiver,
            Expr::ArrayAccess { array, .. } => expr = array,
            Expr::Name { name, span } => {
                return pattern_may_be_another(name, span, files, externals);
            }
            _ => return false,
        }
    }
}

/// Poison each of `own` that `parameter` mentions and this argument did not
/// bind to a known type.
fn poison_unbound(
    parameter: &ExternalType,
    own: &[String],
    found: &std::collections::HashMap<String, Type>,
    poisoned: &mut std::collections::HashSet<String>,
) {
    for name in mentioned(parameter, own) {
        if !found.get(name).is_some_and(Type::is_known) {
            poisoned.insert(name.clone());
        }
    }
}

/// What a lambda passed for a functional-interface parameter can say about a
/// generic method's own variables; `None` where the parameter is not a
/// functional interface this can read.
struct LambdaTarget {
    /// The ones written where its single abstract method takes something.
    inputs: Vec<String>,
    /// The type argument written where it returns something, whose own
    /// variables the body's type is a lower bound for — unless the lambda
    /// takes one of them too, which Java settles before reading the body.
    output: Option<ExternalType>,
    /// How many parameters its single abstract method takes.
    arity: usize,
}

fn lambda_target(
    parameter: &ExternalType,
    own: &[String],
    externals: &dyn Externals,
) -> Option<LambdaTarget> {
    let ExternalType::Parameterized(fqn, arguments) = parameter else {
        return None;
    };
    let interface = externals.members(fqn)?;
    if interface.type_parameters.len() != arguments.len() {
        return None;
    }
    const OBJECT: [&str; 3] = ["equals", "hashCode", "toString"];
    let mut abstracts = interface.members.iter().filter(|member| {
        member.is_abstract && !member.is_static && !OBJECT.contains(&member.name.as_str())
    });
    let (Some(method), None) = (abstracts.next(), abstracts.next()) else {
        return None;
    };
    // The positions of `parameter`'s arguments the interface's variables in
    // `ty` stand for.
    let positions = |ty: &ExternalType| {
        interface
            .type_parameters
            .iter()
            .enumerate()
            .filter(|(_, name)| mentions_variable(ty, name))
            .map(|(at, _)| at)
            .collect::<Vec<_>>()
    };
    let inputs: Vec<String> = method
        .parameters
        .iter()
        .flat_map(&positions)
        .flat_map(|at| mentioned(&arguments[at], own))
        .cloned()
        .collect();
    let output = match &method.declared {
        ExternalType::Variable(returned) => {
            let at = interface
                .type_parameters
                .iter()
                .position(|name| name == returned)?;
            let target = &arguments[at];
            // `UnaryOperator<T>`, `BiFunction<U, ? super T, U>`.
            (!mentioned(target, own)
                .iter()
                .any(|name| inputs.contains(name)))
            .then(|| target.clone())
        }
        declared if positions(declared).is_empty() => None,
        // A return type wrapping a variable is a constraint this does not
        // read, so the lambda is not one it can account for.
        _ => return None,
    };
    Some(LambdaTarget {
        inputs,
        output,
        arity: method.parameters.len(),
    })
}

/// What the left of a method reference names (JLS 6.5.2: an ambiguous name
/// there is a value first, then a type).
enum ReferenceTarget {
    Type(Type),
    Value(Type),
}

/// The left of `reference`, or `None` where it cannot be told a value from
/// a type.
fn reference_target(
    target: &Expr,
    type_text: Option<&str>,
    at: u32,
    files: &Files<'_>,
    externals: &dyn Externals,
    depth: u32,
    work: &mut Work,
) -> Option<ReferenceTarget> {
    if let Some(written) = type_text {
        work.wrote(written, at, files);
        return Some(ReferenceTarget::Type(resolve_written(
            written, files, externals, at,
        )));
    }
    let value = |work: &mut Work| {
        ReferenceTarget::Value(type_at_depth(target, files, externals, depth + 1, work))
    };
    match target {
        Expr::Name { name, span } if !names_a_value(name, span.start_byte, files, externals) => {
            if work.strict() && field_may_shadow(files, externals, name, span.start_byte) {
                work.doubt();
            }
            work.wrote(name, span.start_byte, files);
            Some(ReferenceTarget::Type(resolve_written(
                name,
                files,
                externals,
                span.start_byte,
            )))
        }
        Expr::Name { .. } => Some(value(work)),
        Expr::FieldAccess { receiver, name, .. } => {
            let (root, root_at) = dotted_root(receiver)?;
            if names_a_value(root, root_at, files, externals) {
                return Some(value(work));
            }
            // `Map.Entry`: a type, unless the qualifier is a type with a
            // field of that name, which outranks its member type.
            let qualifier = dotted(receiver)?;
            let qualifier_type = resolve_written(&qualifier, files, externals, root_at);
            if qualifier_type.is_known() {
                let walk = crate::members::walk_members(&qualifier_type, files, externals, true);
                if !walk.closed
                    || walk.members.iter().any(|member| {
                        member.name() == name
                            && matches!(member.kind(), DeclKind::Field | DeclKind::EnumConstant)
                    })
                {
                    return None;
                }
            }
            let written = format!("{qualifier}.{name}");
            let ty = resolve_written(&written, files, externals, root_at);
            work.wrote(&written, root_at, files);
            ty.is_known().then_some(ReferenceTarget::Type(ty))
        }
        _ => Some(value(work)),
    }
}

/// Whether a simple name at `offset` means a value: a variable in scope, a
/// generated field or a statically imported one. Type inference's own order
/// for a bare name, without the type it would go on to read.
fn names_a_value(name: &str, offset: u32, files: &Files<'_>, externals: &dyn Externals) -> bool {
    files.current_scopes().resolve_value(offset, name).is_some()
        || field_owner(files, externals, name, offset).is_some()
        || crate::imports::imported_owner(files, externals, name, Namespace::Value, offset)
            .is_some()
}

/// The first segment of a dotted name and where it is written.
fn dotted_root(expr: &Expr) -> Option<(&str, u32)> {
    match expr {
        Expr::Name { name, span } => Some((name, span.start_byte)),
        Expr::FieldAccess { receiver, .. } => dotted_root(receiver),
        _ => None,
    }
}

/// A dotted name's text, `None` where it is not one.
fn dotted(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Name { name, .. } => Some(name.clone()),
        Expr::FieldAccess { receiver, name, .. } => Some(format!("{}.{name}", dotted(receiver)?)),
        _ => None,
    }
}

/// The type a method reference passed for argument `argument` of `call`
/// returns, read against an interface whose single abstract method takes
/// `arity` parameters (JLS 15.13.1).
///
/// The method is the one [`sole_candidate`] answers for the reference — the
/// one method of the name, as definition on it lands — and it must fit the
/// form: through a type, static with `arity` parameters or an instance
/// method with one fewer (the unbound receiver); through a value, an
/// instance method with `arity`. Neither variadic nor generic. Unbound on a
/// generic type written raw, it is read on the interface's first parameter
/// type where that names the same type. `T::new` is `T`, where `T` is not
/// generic. Unknown wherever any of that fails.
#[allow(clippy::too_many_arguments)]
fn method_ref_result(
    reference: &Expr,
    call: &Member<'_>,
    argument: usize,
    arity: usize,
    files: &Files<'_>,
    externals: &dyn Externals,
    depth: u32,
    work: &mut Work,
) -> Type {
    let unknown = || Type::Unknown(String::new());
    let Expr::MethodRef {
        target,
        type_text,
        name,
        span,
    } = reference
    else {
        return unknown();
    };
    let Some(form) = reference_target(
        target,
        type_text.as_deref(),
        span.start_byte,
        files,
        externals,
        depth,
        work,
    ) else {
        return unknown();
    };
    let (owner, through_type) = match form {
        ReferenceTarget::Type(ty) => (ty, true),
        ReferenceTarget::Value(ty) => (ty, false),
    };
    if !matches!(owner, Type::External { .. } | Type::Declared { .. }) {
        return unknown();
    }
    let Some(name) = name else {
        return if through_type && !raw(&owner, files, externals) {
            owner
        } else {
            unknown()
        };
    };
    // The one method of the name is Java's only where no other could apply:
    // a list that may be short, or a name `Object` declares beside a local
    // overload of it, can hide the one the interface's parameters select.
    if crate::queries::OBJECT_METHODS.contains(&name.as_str()) || !whole(&owner, files, externals) {
        return unknown();
    }
    let Some(member) = candidate_at_depth(
        &owner,
        name,
        Access::MethodRef,
        files,
        externals,
        depth,
        work,
    ) else {
        return unknown();
    };
    if member.kind() != DeclKind::Method || member.is_variadic() || member.is_generic_method() {
        return unknown();
    }
    let count = member.parameter_count();
    let unbound = match (through_type, member.is_static()) {
        (true, true) if count == arity => false,
        (true, false) if count + 1 == arity => true,
        (false, false) if count == arity => false,
        _ => return unknown(),
    };
    let (owner, member) = if unbound && raw(&owner, files, externals) {
        let Some(receiver) = crate::members::lambda_parameter(
            call,
            argument,
            0,
            arity,
            work.strict(),
            files,
            externals,
        ) else {
            return unknown();
        };
        let same = match (&owner, &receiver) {
            (Type::External { fqn, .. }, Type::External { fqn: given, .. }) => fqn == given,
            (Type::Declared { decl, .. }, Type::Declared { decl: given, .. }) => decl == given,
            _ => false,
        };
        if !same {
            return unknown();
        }
        let Some(member) = candidate_at_depth(
            &receiver,
            name,
            Access::MethodRef,
            files,
            externals,
            depth,
            work,
        ) else {
            return unknown();
        };
        // A raw owner's instance methods lose their own variables (JLS 4.8).
        if member.is_generic_method() || member.is_variadic() || member.parameter_count() != count {
            return unknown();
        }
        (receiver, member)
    } else {
        (owner, member)
    };
    work.vouch(&owner, &member, files, externals);
    crate::members::member_type(&member, files, externals)
}

/// Whether `ty` mentions the interface's own variable `name`.
fn mentions_variable(ty: &ExternalType, name: &str) -> bool {
    match ty {
        ExternalType::Variable(variable) => variable == name,
        ExternalType::Parameterized(_, arguments) => arguments
            .iter()
            .any(|argument| mentions_variable(argument, name)),
        ExternalType::Extends(inner) | ExternalType::Super(inner) => mentions_variable(inner, name),
        _ => false,
    }
}

/// What `given` says each of `own` is, read off `parameter`.
fn unify(
    parameter: &ExternalType,
    given: &Type,
    own: &[String],
    externals: &dyn Externals,
    found: &mut std::collections::HashMap<String, Type>,
) {
    match parameter {
        ExternalType::Unresolved(written) if own.iter().any(|name| name == written.trim()) => {
            let value = match given {
                Type::Primitive(primitive) => match externals.boxed(*primitive) {
                    Some(fqn) => Type::External {
                        name: fqn.rsplit('.').next().unwrap_or(&fqn).to_owned(),
                        fqn,
                        arguments: Vec::new(),
                    },
                    None => return,
                },
                Type::External { .. } | Type::Declared { .. } | Type::Array(_) => given.clone(),
                _ => return,
            };
            let name = written.trim().to_owned();
            match found.get(&name) {
                Some(seen) if *seen != value => {
                    // Two answers in one argument: none.
                    found.insert(name, Type::Unknown(String::new()));
                }
                _ => {
                    found.insert(name, value);
                }
            }
        }
        ExternalType::Parameterized(fqn, arguments) => {
            if let Type::External {
                fqn: given_fqn,
                arguments: given_arguments,
                ..
            } = given
                && given_fqn == fqn
                && given_arguments.len() == arguments.len()
            {
                for (parameter, given) in arguments.iter().zip(given_arguments) {
                    unify(parameter, given, own, externals, found);
                }
            }
        }
        // `? extends T` given `X` says `X <: T`, a lower bound like a bare
        // argument's, which is what Java resolves to. `? super T` says only
        // `T <: X`: `max(Collection<? extends T>, Comparator<? super T>)`
        // given a `Comparator<Object>` is no `Object`.
        ExternalType::Extends(inner) => {
            unify(inner, given, own, externals, found);
        }
        _ => {}
    }
}

/// The one member of `owner` called `name` that `access` can only mean.
///
/// `access` picks the namespace — `Value` sees fields, enum constants and
/// nested types; `MethodRef` and `Call` see only methods — and, for `Call`,
/// filters by arity and then by argument type. When several methods still
/// share a name, fit the arity and are not ruled out by any argument, the
/// answer is `None` rather than a pick: a wrong pick is worse than no answer.
/// But a member the type declares itself is not a competing candidate against
/// one it merely inherits — it shadows it, and that is not a guess.
///
/// Shared by `member_type`, `queries::member_definition_at` and
/// `queries::call_definition_at`, so a qualified and an unqualified access use
/// one filter rather than two that could drift and disagree.
pub(crate) fn sole_candidate<'a>(
    owner: &Type,
    name: &str,
    access: Access<'_>,
    files: &Files<'a>,
    externals: &dyn Externals,
) -> Option<Member<'a>> {
    let mut work = Work::new(false);
    candidate_at_depth(owner, name, access, files, externals, 0, &mut work)
}

/// [`sole_candidate`] on the inference recursion's own two bounds, because
/// typing an argument re-enters this function through `type_at_depth`.
fn candidate_at_depth<'a>(
    owner: &Type,
    name: &str,
    access: Access<'_>,
    files: &Files<'a>,
    externals: &dyn Externals,
    depth: u32,
    work: &mut Work,
) -> Option<Member<'a>> {
    // Candidates, not types. Resolving a member's type walks the whole file
    // (see `members::member_type`), and the name and arity filters below throw
    // all but one or two of these away — so the resolution happens once, on the
    // winner, after they have run.
    let mut matches: Vec<Member<'a>> = Vec::new();
    // ponytail: both passes run regardless of whether `owner` came from a
    // value or a type name, so a type-name receiver is offered an instance
    // member too. Only misfires on Java that would not compile (an instance
    // member is not reachable through a bare type name), consistent with this
    // phase having no diagnostics; narrow by receiver kind if that ever bites.
    // `Access::Value` likewise puts fields, enum constants and nested types in
    // one match set, so a field and a same-named nested type refuse together
    // (`hides_the_rest` will not let one hide the other) where the position
    // would pick one; the same upgrade, telling a type position from a value
    // one, lifts it.
    for statics in [false, true] {
        for member in members_of(owner, files, externals, statics) {
            if member.name() != name {
                continue;
            }
            let wants_method = !matches!(access, Access::Value);
            // An annotation type's element is a method too (JLS 9.6.1).
            let is_method = matches!(
                member.kind(),
                DeclKind::Method | DeclKind::AnnotationElement
            );
            if wants_method != is_method {
                continue;
            }
            if let Access::Call { arguments } = access
                && !fits(&member, arguments.len())
            {
                continue;
            }
            matches.push(member);
        }
    }
    narrow(owner, name, access, matches, files, externals, depth, work)
}

/// Whether a call of `arity` arguments can reach `member` at all.
fn fits(member: &Member<'_>, arity: usize) -> bool {
    let declared = member.parameter_count();
    if member.is_variadic() {
        // A varargs method takes its fixed parameters plus any number of
        // trailing arguments, so one fewer argument than parameters is the
        // floor: `m(String a, int... xs)` still needs the `a`.
        arity + 1 >= declared
    } else {
        arity == declared
    }
}

/// The constructor `new ty(arguments)` selects, through the filters a
/// method call goes through — over a list [`crate::members::constructors_of`]
/// only answers where it is whole, so `most_specific` may rank it.
pub(crate) fn sole_constructor<'a>(
    ty: &Type,
    arguments: &[Expr],
    files: &Files<'a>,
    externals: &dyn Externals,
    depth: u32,
    work: &mut Work,
) -> Option<Member<'a>> {
    let mut matches = crate::members::constructors_of(ty, files, externals)?;
    matches.retain(|member| fits(member, arguments.len()));
    // JLS 15.12.2.1 drops a constructor the call cannot see before fixed
    // arity is preferred, so one that may be hidden from here must not push
    // a variadic sibling out.
    if matches.iter().any(Member::is_variadic)
        && matches
            .iter()
            .any(|member| !member.is_variadic() && !member.visible_from(files))
    {
        return None;
    }
    let access = Access::Call { arguments };
    narrow(ty, "", access, matches, files, externals, depth, work)
}

/// `matches`, every member of `owner` called `name` that the access can
/// reach, narrowed to the one Java selects or to nothing. An empty `name` is
/// a constructor's: the list is whole by construction and declares nothing
/// `Object` does.
#[allow(clippy::too_many_arguments)]
fn narrow<'a>(
    owner: &Type,
    name: &str,
    access: Access<'_>,
    mut matches: Vec<Member<'a>>,
    files: &Files<'a>,
    externals: &dyn Externals,
    depth: u32,
    work: &mut Work,
) -> Option<Member<'a>> {
    // Elimination by argument type: JLS 15.12.2.2's applicability by subtyping
    // and by widening primitive conversion, as far as the reachable files and
    // the world demonstrate it.
    // See `apply.rs` for which exclusions are certain and why the rest are not.
    //
    // Ahead of the phase ordering below, so a fixed-arity candidate the
    // arguments rule out does not push out the variadic one Java takes:
    // slf4j's `info("{} {} {}", a, b, c)` is `info(String, Object...)`, not
    // `info(Marker, String, Object, Object)`.
    //
    // Only with more than one candidate left: typing an argument walks the
    // file, and the single-candidate case is both the common one and already
    // answered. A lone candidate the arguments contradict therefore still
    // answers — the buffer is mid-edit far more often than it is wrong about
    // which method it means.
    let mut argument_types: Option<Vec<Type>> = None;
    if let Access::Call { arguments } = access
        && matches.len() > 1
    {
        // Typed once each, ahead of the candidates rather than inside the
        // filter: an argument's type does not depend on which candidate is
        // being tested, and typing it per candidate is that file walk times
        // the overload count on the request loop.
        let types: Vec<Type> = arguments
            .iter()
            .map(|argument| match work.left.checked_sub(1) {
                Some(left) => {
                    work.left = left;
                    type_at_depth(argument, files, externals, depth + 1, work)
                }
                None => Type::Unknown(String::new()),
            })
            .collect();
        matches.retain(|member| !excluded_by_arguments(member, &types, files, externals));
        argument_types = Some(types);
    }
    // JLS 15.12.2's phase ordering: applicability by fixed arity is tried
    // before applicability by variable arity, so `f(1, 2)` against
    // `f(int, int)` and `f(int, int...)` compiles to the fixed one. A fact
    // about Java, not a preference — variadic candidates lose to any
    // fixed-arity survivor at the same count rather than competing with it.
    //
    // Except that those two phases read a variadic method of n parameters,
    // called with n arguments, as taking the array itself: `List.of(array)`
    // is `of(E...)` with `E` the element type, not `of(E)` with `E` the
    // array. So a variadic candidate of that arity keeps competing whenever
    // the last argument may be an array.
    if let Access::Call { arguments } = access
        && matches.iter().any(|member| !member.is_variadic())
    {
        let tail_competes = matches
            .iter()
            .any(|member| member.is_variadic() && member.parameter_count() == arguments.len())
            && arguments
                .last()
                .is_some_and(|last| match work.left.checked_sub(1) {
                    Some(left) => {
                        work.left = left;
                        matches!(
                            type_at_depth(last, files, externals, depth + 1, work),
                            Type::Array(_) | Type::Null | Type::Unknown(_)
                        )
                    }
                    None => true,
                });
        matches.retain(|member| {
            !member.is_variadic() || (tail_competes && member.parameter_count() == arguments.len())
        });
    }
    // A member the type declares itself shadows an inherited one of the same
    // name, whichever namespace each landed in: `class Sub extends Base`
    // re-declaring `X` hides `Base`'s `X`, and that access is unambiguous in
    // Java. Without this, both match and this function reads an unambiguous
    // access as ambiguous.
    //
    // Two conditions, and both are the rule rather than one being a guard on
    // it. There must be exactly one candidate on the type itself — two
    // same-arity overloads it declares are genuine ambiguity, whether or not
    // anything is inherited. And that one must *hide* the others rather than
    // compete with them, which is `hides_the_rest`: hiding is what an override
    // does, and an override has the parameter types of what it overrides.
    // Without that second half the rule narrows by where a member was
    // declared, which the code cannot demonstrate — `Sub.f(Bar)` against
    // `Base.f(Foo)` is an overload set, and picking `Sub`'s because it is
    // nearer is exactly the guess this phase refuses.
    //
    // Only past one survivor: with a single match the arm below answers it,
    // and `hides_the_rest` resolves parameter types, which walks the file.
    if matches.len() > 1 {
        let own: Vec<&Member<'a>> = matches
            .iter()
            .filter(|member| member.declared_by(owner))
            .collect();
        if own.len() == 1 && hides_the_rest(own[0], &matches, files, externals) {
            return Some(own[0].clone());
        }
        // The same rule one step out: every candidate came from one external
        // supertype's closure — `class Names extends ArrayList<String>`'s —
        // and that type declares one of them itself, which hides what it
        // inherits. One closure and not several, because across two Java's
        // answer turns on which is a class and which an interface's default,
        // and the facts this sees carry no kind.
        if own.is_empty()
            && let Some(closure) = matches[0].closure()
            && matches
                .iter()
                .all(|member| member.closure() == Some(closure))
        {
            let nearest: Vec<&Member<'a>> = matches
                .iter()
                .filter(|member| member.nearest_closure().is_some())
                .collect();
            if nearest.len() == 1 && hides_the_rest(nearest[0], &matches, files, externals) {
                return Some(nearest[0].clone());
            }
        }
    }
    // JLS 15.12.2.5 where it is provable — see `most_specific` — and only
    // over a list the walk shows whole, since a candidate it never reached
    // could be the more specific one; and never for a name `Object` declares,
    // the one part of a local type's list no walk holds.
    //
    // ponytail: "whole" is the walk's, which knows nothing of a tool that
    // adds members — Lombok's `@Delegate`, an AspectJ inter-type declaration
    // — so a more specific member one of them writes is not ranked against.
    // The upgrade is the diagnostic's `BuildHints::rewrites` gate reaching
    // this query through `Externals`.
    if matches.len() > 1
        && let Some(types) = &argument_types
        && !crate::queries::OBJECT_METHODS.contains(&name)
        && let Some(winner) = most_specific(&matches, types, files, externals)
        && (name.is_empty()
            || [false, true].into_iter().all(|statics| {
                crate::members::walk_members(owner, files, externals, statics).whole_but_object
            }))
    {
        return Some(winner.clone());
    }
    match matches.as_slice() {
        [only] => Some(only.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoExternals;
    use crate::externals::ExternalType;
    use crate::externals::testing::{World, class, method};
    use crate::files::FileId;
    use javelin_document::{DocumentId, TextDocument};
    use javelin_parser::{
        FileSyntax, JavaParser, ScopeTree, SelectionKind, Span, member_selection_at,
    };

    /// The one file every fixture below is, named the same way its document
    /// is so that a `Files` set and a `TextDocument` agree on who they are.
    const FILE: &str = "file:///A.java";

    fn set<'a>(syntax: &'a FileSyntax, scopes: &'a ScopeTree) -> Files<'a> {
        Files::single(FileId::new(FILE), syntax, scopes)
    }

    /// Type the receiver of the member selection marked with `|`.
    fn receiver_type(marked: &str) -> Type {
        receiver_type_with(marked, &NoExternals)
    }

    /// The same against a world that holds something, for the receivers whose
    /// type only a classpath settles.
    fn receiver_type_with(marked: &str, externals: &dyn Externals) -> Type {
        let offset = marked.find('|').expect("mark the cursor with |") as u32;
        let source = marked.replace('|', "");
        let document = TextDocument::new(DocumentId::new(FILE), 1, source);
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let scopes = ScopeTree::build(&document, &tree);
        let selection = member_selection_at(&document, &tree, offset).expect("a selection");
        type_of(&selection.receiver, &set(&syntax, &scopes), externals)
    }

    /// The one external member the marked selection can mean, named by its
    /// rendered signature — which is how an external winner is identified at
    /// all, since it has neither a span nor any written text.
    ///
    /// Through `sole_candidate` and a real parse, so every filter between an
    /// access and the member that wins is in the answer: the namespace, the
    /// arity, fixed-before-variadic, elimination by argument type, and
    /// own-shadows-inherited.
    fn sole_external(marked: &str, externals: &dyn Externals) -> Option<String> {
        let offset = marked.find('|').expect("mark the cursor with |") as u32;
        let source = marked.replace('|', "");
        let document = TextDocument::new(DocumentId::new(FILE), 1, source);
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let scopes = ScopeTree::build(&document, &tree);
        let selection = member_selection_at(&document, &tree, offset).expect("a selection");
        let files = set(&syntax, &scopes);
        let owner = type_of(&selection.receiver, &files, externals);
        let access = match &selection.kind {
            SelectionKind::Value => Access::Value,
            SelectionKind::MethodRef => Access::MethodRef,
            SelectionKind::Call { arguments } => Access::Call { arguments },
        };
        let found = sole_candidate(&owner, &selection.name, access, &files, externals)?;
        let facts = found.external()?;
        Some(format!("{} on {}", facts.signature(), facts.owner))
    }

    const STRING: &str = "java.lang.String";
    const OBJECT: &str = "java.lang.Object";
    const INTEGER: &str = "java.lang.Integer";

    /// A world holding `String`, `Object` and `Integer`, with whatever members
    /// the caller gives `String`. The two box facts are what argument-type
    /// elimination needs to say an `Integer` is not a `String`.
    fn string_world(members: Vec<crate::ExternalMember>) -> World {
        World::holding(&[OBJECT])
            .with(INTEGER, &[OBJECT], Vec::new())
            .with(STRING, &[OBJECT], members)
    }

    /// A `String` local, and a `.` on it: the shape every test below drives,
    /// and the only way to a receiver the open file cannot declare.
    fn on_a_string(access: &str) -> String {
        format!("class A {{ void m(String s) {{ s.{access}; }} }}")
    }

    #[test]
    fn a_chained_call_through_an_external_return_type_finds_its_members() {
        // The receiver of the second `.` is the *return type* of an external
        // member, so this only answers if a fact's type is resolved well
        // enough to be asked about in turn. Before this slice the chain ended
        // at `s`.
        let world = string_world(vec![
            method(STRING, "trim", class(STRING), vec![]),
            method(
                STRING,
                "length",
                ExternalType::Primitive(Primitive::Int),
                vec![],
            ),
        ]);
        assert_eq!(
            sole_external(&on_a_string("trim().length|()"), &world),
            Some("int length() on java.lang.String".to_owned())
        );
        // And the intermediate type is the fact's, not the receiver's by
        // accident: `trim()` is a `java.lang.String` because that is what the
        // fact says, which is what the line above then reads members off.
        let chained = receiver_type_with(&on_a_string("trim().|"), &world);
        assert_eq!(
            chained,
            Type::External {
                name: "String".into(),
                fqn: STRING.into(),
                arguments: Vec::new(),
            }
        );
    }

    #[test]
    fn two_external_overloads_the_arity_separates_resolve_to_one() {
        let int = || ExternalType::Primitive(Primitive::Int);
        let world = string_world(vec![
            method(STRING, "substring", class(STRING), vec![int()]),
            method(STRING, "substring", class(STRING), vec![int(), int()]),
        ]);
        assert_eq!(
            sole_external(&on_a_string("substring|(1)"), &world),
            Some("String substring(int) on java.lang.String".to_owned())
        );
        assert_eq!(
            sole_external(&on_a_string("substring|(1, 2)"), &world),
            Some("String substring(int, int) on java.lang.String".to_owned())
        );
    }

    #[test]
    fn of_two_external_overloads_both_applicable_the_more_specific_is_picked() {
        // `1` is applicable to `int` directly and to `long` by widening, so
        // elimination leaves both standing; `int` is the more specific (JLS
        // 15.12.2.5), for an external candidate exactly as for a local one.
        let world = string_world(vec![
            method(
                STRING,
                "f",
                class(STRING),
                vec![ExternalType::Primitive(Primitive::Int)],
            ),
            method(
                STRING,
                "f",
                class(STRING),
                vec![ExternalType::Primitive(Primitive::Long)],
            ),
        ]);
        let picked = sole_external(&on_a_string("f|(1)"), &world).expect("the int overload");
        assert!(picked.contains("f(int)"), "{picked}");
    }

    #[test]
    fn an_external_overload_an_argument_type_rules_out_is_eliminated() {
        // `apply.rs` needed no new arm for this: `(Primitive, External)`
        // already boxes the `int` and asks the world, and `Some(false)` from
        // it is what eliminates `f(String)`. The pair of overloads is
        // otherwise identical in arity, so nothing else could have separated
        // them.
        let world = string_world(vec![
            method(
                STRING,
                "f",
                class(STRING),
                vec![ExternalType::Primitive(Primitive::Int)],
            ),
            method(STRING, "f", class(STRING), vec![class(STRING)]),
        ]);
        assert_eq!(
            sole_external(&on_a_string("f|(1)"), &world),
            Some("String f(int) on java.lang.String".to_owned())
        );
        // Not vacuous: the same pair against a world that holds no box fact
        // for `int` cannot exclude either, and refuses.
        let unboxable = World::default().with(
            STRING,
            &[],
            vec![
                method(
                    STRING,
                    "f",
                    class(STRING),
                    vec![ExternalType::Primitive(Primitive::Int)],
                ),
                method(STRING, "f", class(STRING), vec![class(STRING)]),
            ],
        );
        assert!(sole_external(&on_a_string("f|(1)"), &unboxable).is_none());
    }

    /// `StringBuilder.append(AbstractStringBuilder)` is package-private and
    /// the most specific for a `StringBuilder` argument; Java, outside
    /// `java.lang`, calls `append(CharSequence)`. A fact not known public
    /// never wins the ranking.
    #[test]
    fn a_most_specific_external_winner_not_known_public_answers_nothing() {
        let world = |public: bool| {
            let mut narrow = method(STRING, "f", class(STRING), vec![class(STRING)]);
            narrow.is_public = public;
            string_world(vec![
                narrow,
                method(STRING, "f", class(STRING), vec![class(OBJECT)]),
            ])
        };
        let call = on_a_string("f|(s)");
        assert!(sole_external(&call, &world(false)).is_none());
        assert_eq!(
            sole_external(&call, &world(true)),
            Some("String f(String) on java.lang.String".to_owned())
        );
    }

    /// A workspace member's `List<String>` arrives with its argument
    /// unresolved. That proves nothing about a `List<Integer>`, which Java
    /// cannot pass to it at all: `f(Object)` is its pick, and neither is ours.
    #[test]
    fn a_parameter_with_unresolved_arguments_is_never_proven_most_specific() {
        const LIST: &str = "java.util.List";
        let world = |argument: ExternalType| {
            string_world(vec![
                method(
                    STRING,
                    "f",
                    class(STRING),
                    vec![ExternalType::Parameterized(LIST.into(), vec![argument])],
                ),
                method(STRING, "f", class(STRING), vec![class(OBJECT)]),
            ])
            .with(LIST, &[OBJECT], Vec::new())
            .generic(&["E"])
        };
        let call = "class A { void m(String s, List<Integer> ints) { s.f|(ints); } }";
        assert!(sole_external(call, &world(ExternalType::Unresolved("String".into()))).is_none());
        assert!(
            sole_external(call, &world(class(INTEGER))).is_some(),
            "a resolved, identical argument still proves it"
        );
    }

    #[test]
    fn an_external_member_the_receivers_own_type_declares_shadows_an_inherited_one() {
        // `String.toString()` against `Object.toString()`: two candidates of
        // one name and one arity, which is an ambiguity until the rule that
        // an override hides what it overrides is applied — and applying it
        // needs the fact's `owner`, which is why the trait demands one.
        let world = string_world(vec![
            method(STRING, "toString", class(STRING), vec![]),
            method(OBJECT, "toString", class(OBJECT), vec![]),
        ]);
        assert_eq!(
            sole_external(&on_a_string("toString|()"), &world),
            Some("String toString() on java.lang.String".to_owned())
        );
        // Not vacuous, and in the direction that matters: with neither
        // candidate declared by the receiver's own type there is nothing to
        // shadow, and two survivors answer nothing rather than the nearer one.
        let inherited = string_world(vec![
            method(OBJECT, "toString", class(OBJECT), vec![]),
            method(INTEGER, "toString", class(STRING), vec![]),
        ]);
        assert!(sole_external(&on_a_string("toString|()"), &inherited).is_none());
    }

    /// `class Names extends ArrayList<String>` inherits `ArrayList.get` and
    /// the `List.get` it overrides, and declares neither: the external
    /// supertype's own override hides what it inherits, as the receiver's own
    /// would — and only when it really overrides it.
    #[test]
    fn an_external_supertypes_own_override_hides_what_it_inherits() {
        const ARRAY_LIST: &str = "java.util.ArrayList";
        const LIST: &str = "java.util.List";
        let int = || ExternalType::Primitive(Primitive::Int);
        let element = || ExternalType::Variable("E".into());
        let world = string_world(Vec::new())
            .with(
                ARRAY_LIST,
                &[LIST],
                vec![
                    method(ARRAY_LIST, "get", element(), vec![int()]),
                    method(LIST, "get", element(), vec![int()]),
                    method(LIST, "of", element(), vec![int()]),
                    method(LIST, "of", element(), vec![class(STRING)]),
                ],
            )
            .generic(&["E"]);
        let local = |access: &str| {
            format!(
                "import java.util.ArrayList; class Names extends ArrayList<String> {{}} \
                 class A {{ void m(Names n) {{ n.{access}; }} }}"
            )
        };
        assert_eq!(
            sole_external(&local("get|(0)"), &world),
            Some("E get(int) on java.util.ArrayList".to_owned())
        );
        // Two overloads neither of which the supertype overrides still
        // answer nothing.
        assert!(sole_external(&local("of|(x)"), &world).is_none());
        // Candidates from two closures — a class's and an interface's — are
        // not one override chain this can see, so nothing is picked.
        let world = world.with(
            "p.Getter",
            &[],
            vec![method("p.SuperGetter", "get", element(), vec![int()])],
        );
        let mixed = "import java.util.ArrayList; \
                     class Mixed extends ArrayList<String> implements Getter {} \
                     class A { void m(Mixed n) { n.get|(0); } }";
        assert!(sole_external(mixed, &world).is_none());
    }

    /// `List<E>.forEach(Consumer<? super E>)`, `Map<K, V>.forEach` with a
    /// `BiConsumer`, and the functional interfaces' single abstract methods
    /// beside a default one and an `equals` every interface may redeclare.
    fn lambda_world() -> World {
        const LIST: &str = "java.util.List";
        const MAP: &str = "java.util.Map";
        const CONSUMER: &str = "java.util.function.Consumer";
        const BI_CONSUMER: &str = "java.util.function.BiConsumer";
        let variable = |name: &str| ExternalType::Variable(name.into());
        let parameterized =
            |fqn: &str, arguments| ExternalType::Parameterized(fqn.into(), arguments);
        let lower = |inner| ExternalType::Super(Box::new(inner));
        let upper = |inner| ExternalType::Extends(Box::new(inner));
        let abstract_method = |owner: &str, name: &str, parameters| {
            let mut found = method(owner, name, ExternalType::Void, parameters);
            found.is_abstract = true;
            found
        };
        string_world(Vec::new())
            .with(
                LIST,
                &[],
                vec![
                    method(
                        LIST,
                        "forEach",
                        ExternalType::Void,
                        vec![parameterized(CONSUMER, vec![lower(variable("E"))])],
                    ),
                    method(
                        LIST,
                        "loosely",
                        ExternalType::Void,
                        vec![parameterized(CONSUMER, vec![upper(variable("E"))])],
                    ),
                    method(
                        LIST,
                        "twice",
                        ExternalType::Void,
                        vec![parameterized(CONSUMER, vec![variable("E")])],
                    ),
                    method(
                        LIST,
                        "twice",
                        ExternalType::Void,
                        vec![parameterized(CONSUMER, vec![class(STRING)])],
                    ),
                ],
            )
            .generic(&["E"])
            .with(
                MAP,
                &[],
                vec![method(
                    MAP,
                    "forEach",
                    ExternalType::Void,
                    vec![parameterized(
                        BI_CONSUMER,
                        vec![lower(variable("K")), lower(variable("V"))],
                    )],
                )],
            )
            .generic(&["K", "V"])
            .with(
                CONSUMER,
                &[],
                vec![
                    abstract_method(CONSUMER, "accept", vec![variable("T")]),
                    abstract_method(CONSUMER, "equals", vec![class(OBJECT)]),
                    method(
                        CONSUMER,
                        "andThen",
                        parameterized(CONSUMER, vec![variable("T")]),
                        vec![parameterized(CONSUMER, vec![lower(variable("T"))])],
                    ),
                ],
            )
            .generic(&["T"])
            .with(
                BI_CONSUMER,
                &[],
                vec![abstract_method(
                    BI_CONSUMER,
                    "accept",
                    vec![variable("T"), variable("U")],
                )],
            )
            .generic(&["T", "U"])
    }

    fn lambda_in(body: &str) -> Type {
        receiver_type_with(
            &format!(
                "import java.util.List; import java.util.Map; \
                 class A {{ void m(List<String> l, List raw, Map<String, Integer> counts) {{ {body} }} }}"
            ),
            &lambda_world(),
        )
    }

    fn external_named(fqn: &str) -> Type {
        Type::External {
            name: fqn.rsplit('.').next().unwrap_or(fqn).into(),
            fqn: fqn.into(),
            arguments: Vec::new(),
        }
    }

    #[test]
    fn an_implicit_lambda_parameter_is_the_functional_interfaces_parameter() {
        assert_eq!(
            lambda_in("l.forEach(x -> x.|length());"),
            external_named(STRING)
        );
        assert_eq!(
            lambda_in("counts.forEach((k, v) -> v.|intValue());"),
            external_named(INTEGER)
        );
        assert_eq!(
            lambda_in("counts.forEach((k, v) -> k.|length());"),
            external_named(STRING)
        );
        assert_eq!(
            lambda_in("l.forEach((String s) -> s.|length());"),
            external_named(STRING)
        );
    }

    /// A world with generic *methods*: `Optional.of`, `List.of`, a stream's
    /// `map`, `Objects.requireNonNull`.
    fn generic_world() -> World {
        const OPTIONAL: &str = "java.util.Optional";
        const LIST: &str = "java.util.List";
        const STREAM: &str = "java.util.stream.Stream";
        const FUNCTION: &str = "java.util.function.Function";
        const OBJECTS: &str = "java.util.Objects";
        let own = |name: &str| ExternalType::Unresolved(name.into());
        let variable = |name: &str| ExternalType::Variable(name.into());
        let parameterized =
            |fqn: &str, arguments| ExternalType::Parameterized(fqn.into(), arguments);
        let generic = |mut found: crate::ExternalMember, names: &[&str], is_static| {
            found.type_parameters = names.iter().map(|name| (*name).to_owned()).collect();
            found.is_static = is_static;
            found
        };
        let mut list_of = generic(
            method(
                LIST,
                "of",
                parameterized(LIST, vec![own("E")]),
                vec![own("E")],
            ),
            &["E"],
            true,
        );
        list_of.variadic = true;
        let mut apply = method(FUNCTION, "apply", variable("R"), vec![variable("T")]);
        apply.is_abstract = true;
        const UNARY: &str = "java.util.function.UnaryOperator";
        const SET: &str = "java.util.Set";
        let mut unary_apply = method(UNARY, "apply", variable("T"), vec![variable("T")]);
        unary_apply.is_abstract = true;
        let mut set_of = generic(
            method(
                SET,
                "of",
                parameterized(SET, vec![own("E")]),
                vec![own("E")],
            ),
            &["E"],
            true,
        );
        set_of.variadic = true;
        const COLLECTOR: &str = "java.util.stream.Collector";
        const COLLECTORS: &str = "java.util.stream.Collectors";
        const MAP: &str = "java.util.Map";
        let wild = || own("?");
        // `Collector<T, ?, R>` for a collector of `T`s into an `R`.
        let collector = |element, result| parameterized(COLLECTOR, vec![element, wild(), result]);
        // `<R, A> R name(Collector<? super T, A, R>)`, or `? extends T`.
        let collect = |name: &str, element| {
            generic(
                method(
                    STREAM,
                    name,
                    own("R"),
                    vec![parameterized(COLLECTOR, vec![element, own("A"), own("R")])],
                ),
                &["R", "A"],
                false,
            )
        };
        let collectors = vec![
            generic(
                method(
                    COLLECTORS,
                    "toSet",
                    collector(own("T"), parameterized(SET, vec![own("T")])),
                    vec![],
                ),
                &["T"],
                true,
            ),
            generic(
                method(
                    COLLECTORS,
                    "toList",
                    collector(own("T"), parameterized(LIST, vec![own("T")])),
                    vec![],
                ),
                &["T"],
                true,
            ),
            method(
                COLLECTORS,
                "joining",
                collector(class("java.lang.CharSequence"), class(STRING)),
                vec![],
            ),
            generic(
                method(
                    COLLECTORS,
                    "groupingBy",
                    collector(
                        own("T"),
                        parameterized(MAP, vec![own("K"), parameterized(LIST, vec![own("T")])]),
                    ),
                    vec![parameterized(
                        FUNCTION,
                        vec![
                            ExternalType::Super(Box::new(own("T"))),
                            ExternalType::Extends(Box::new(own("K"))),
                        ],
                    )],
                ),
                &["T", "K"],
                true,
            ),
            // A collector whose `T` its argument binds.
            generic(
                method(
                    COLLECTORS,
                    "sample",
                    collector(own("T"), parameterized(LIST, vec![own("T")])),
                    vec![own("T")],
                ),
                &["T"],
                true,
            ),
            // A collector whose `T` a typed lambda's parameter writes.
            generic(
                method(
                    COLLECTORS,
                    "listing",
                    collector(own("T"), parameterized(LIST, vec![own("T")])),
                    vec![parameterized(FUNCTION, vec![own("T"), class(STRING)])],
                ),
                &["T"],
                true,
            ),
        ];
        let mut collectors = collectors;
        for found in &mut collectors {
            found.is_static = true;
        }
        string_world(vec![method(STRING, "trim", class(STRING), vec![])])
            .with("java.lang.Long", &[OBJECT], Vec::new())
            .with(
                OPTIONAL,
                &[OBJECT],
                vec![
                    generic(
                        method(
                            OPTIONAL,
                            "of",
                            parameterized(OPTIONAL, vec![own("T")]),
                            vec![own("T")],
                        ),
                        &["T"],
                        true,
                    ),
                    method(OPTIONAL, "get", variable("T"), vec![]),
                ],
            )
            .generic(&["T"])
            .with(
                LIST,
                &[OBJECT],
                vec![
                    list_of,
                    method(
                        LIST,
                        "stream",
                        parameterized(STREAM, vec![variable("E")]),
                        vec![],
                    ),
                    generic(
                        method(LIST, "pick", own("U"), vec![own("U")]),
                        &["U"],
                        false,
                    ),
                    generic(
                        method(LIST, "first", parameterized(LIST, vec![own("U")]), vec![]),
                        &["U"],
                        false,
                    ),
                ],
            )
            .generic(&["E"])
            .with(
                STREAM,
                &[OBJECT],
                vec![
                    generic(
                        method(
                            STREAM,
                            "map",
                            parameterized(STREAM, vec![own("R")]),
                            vec![parameterized(
                                FUNCTION,
                                vec![
                                    ExternalType::Super(Box::new(variable("T"))),
                                    ExternalType::Extends(Box::new(own("R"))),
                                ],
                            )],
                        ),
                        &["R"],
                        false,
                    ),
                    collect("collect", ExternalType::Super(Box::new(variable("T")))),
                    collect("gather", ExternalType::Extends(Box::new(variable("T")))),
                ],
            )
            .generic(&["T"])
            .with(COLLECTOR, &[OBJECT], Vec::new())
            .generic(&["T", "A", "R"])
            .with(COLLECTORS, &[OBJECT], collectors)
            .with(MAP, &[OBJECT], Vec::new())
            .generic(&["K", "V"])
            .with("java.lang.CharSequence", &[OBJECT], Vec::new())
            .with(FUNCTION, &[OBJECT], vec![apply])
            .generic(&["T", "R"])
            .with(
                "java.util.Collections",
                &[OBJECT],
                vec![generic(
                    method(
                        "java.util.Collections",
                        "max",
                        own("T"),
                        vec![
                            parameterized(
                                "java.util.Collection",
                                vec![ExternalType::Extends(Box::new(own("T")))],
                            ),
                            parameterized(
                                "java.util.Comparator",
                                vec![ExternalType::Super(Box::new(own("T")))],
                            ),
                        ],
                    ),
                    &["T"],
                    true,
                )],
            )
            .with("java.util.Comparator", &[OBJECT], Vec::new())
            .generic(&["T"])
            .with(
                OBJECTS,
                &[OBJECT],
                vec![
                    generic(
                        method(OBJECTS, "requireNonNull", own("T"), vec![own("T")]),
                        &["T"],
                        true,
                    ),
                    generic(
                        method(
                            OBJECTS,
                            "feed",
                            parameterized(OPTIONAL, vec![own("T")]),
                            vec![
                                own("T"),
                                parameterized(FUNCTION, vec![own("T"), class(STRING)]),
                            ],
                        ),
                        &["T"],
                        true,
                    ),
                    generic(
                        method(
                            OBJECTS,
                            "either",
                            parameterized(OPTIONAL, vec![own("T")]),
                            vec![
                                own("T"),
                                parameterized(FUNCTION, vec![class(STRING), own("T")]),
                            ],
                        ),
                        &["T"],
                        true,
                    ),
                    generic(
                        method(
                            OBJECTS,
                            "fold",
                            parameterized(OPTIONAL, vec![own("T")]),
                            vec![parameterized(UNARY, vec![own("T")])],
                        ),
                        &["T"],
                        true,
                    ),
                ],
            )
            .with(UNARY, &[OBJECT], vec![unary_apply])
            .generic(&["T"])
            .with(
                SET,
                &[OBJECT],
                vec![
                    generic(
                        method(
                            SET,
                            "of",
                            parameterized(SET, vec![own("E")]),
                            vec![own("E")],
                        ),
                        &["E"],
                        true,
                    ),
                    set_of,
                    method(
                        SET,
                        "stream",
                        parameterized(STREAM, vec![variable("E")]),
                        vec![],
                    ),
                ],
            )
            .generic(&["E"])
    }

    fn inferred(body: &str) -> Type {
        receiver_type_with(
            &format!(
                "import java.util.*; \
                 class A {{ void m(List<String> l, String s, long n, String[] a, \
                 Comparator<Object> c, List raw) {{ {body} }} }}"
            ),
            &generic_world(),
        )
    }

    fn generic_of(fqn: &str, argument: Type) -> Type {
        Type::External {
            name: fqn.rsplit('.').next().unwrap_or(fqn).into(),
            fqn: fqn.into(),
            arguments: vec![argument],
        }
    }

    /// A generic method's own type variable is what the call passes for it:
    /// as it is, boxed, through a type argument, or as a lambda's body.
    #[test]
    fn a_generic_methods_own_variable_is_bound_by_what_the_call_passes() {
        assert_eq!(
            inferred("Optional.of(s).|get();"),
            generic_of("java.util.Optional", external_named(STRING))
        );
        assert_eq!(
            inferred("Objects.requireNonNull(s).|trim();"),
            external_named(STRING)
        );
        assert_eq!(
            inferred("Optional.of(n).|get();"),
            generic_of("java.util.Optional", external_named("java.lang.Long")),
            "a primitive argument is its box"
        );
        assert_eq!(
            inferred("List.of(s, s).|stream();"),
            generic_of("java.util.List", external_named(STRING))
        );
        assert_eq!(
            inferred("l.stream().map(z -> z.trim()).|count();"),
            generic_of("java.util.stream.Stream", external_named(STRING)),
            "the lambda's body types what `map` returns"
        );
        assert_eq!(
            inferred("l.stream().map((String z) -> z.trim()).|count();"),
            generic_of("java.util.stream.Stream", external_named(STRING)),
            "a typed lambda's body still types what it returns"
        );
        assert_eq!(
            inferred("Objects.feed(s, o -> s).|get();"),
            generic_of("java.util.Optional", external_named(STRING)),
            "an implicit lambda takes what the other arguments settle"
        );
        assert_eq!(
            inferred("Objects.either(s, z -> s).|get();"),
            generic_of("java.util.Optional", external_named(STRING))
        );
        assert_eq!(
            inferred("List.of(s, null, s).|stream();"),
            generic_of("java.util.List", external_named(STRING)),
            "`null` constrains nothing"
        );
        assert_eq!(
            inferred("List.of(a).|stream();"),
            generic_of("java.util.List", external_named(STRING)),
            "one array for `E...` is the array itself, of `E`s"
        );
        assert_eq!(
            inferred("Set.of(s).|stream();"),
            generic_of("java.util.Set", external_named(STRING))
        );
        assert!(
            !inferred("Set.of(a).|stream();").is_known(),
            "`of(E)` and `of(E...)` both take one array, and the variadic one is Java's"
        );
    }

    /// A block lambda's own `return`s type what the interface returns, when
    /// every one that is not `null` has one type.
    #[test]
    fn a_block_lambdas_returns_bind_what_the_interface_returns() {
        let string = || generic_of("java.util.stream.Stream", external_named(STRING));
        for (body, why) in [
            (
                "l.stream().map(z -> { return z.trim(); }).|count();",
                "one return",
            ),
            (
                "l.stream().map(z -> { String t = z.trim(); return t; }).|count();",
                "a local the block declares",
            ),
            (
                "l.stream().map(z -> { var t = z.trim(); return t; }).|count();",
                "a `var` the block declares",
            ),
            (
                "l.stream().map(z -> { if (z == null) return s; return z.trim(); }).|count();",
                "two returns of one type",
            ),
            (
                "l.stream().map(z -> { if (z == null) return null; return z; }).|count();",
                "`null` constrains nothing",
            ),
            (
                "l.stream().map(z -> { Objects.feed(s, o -> { return n; }); return z; }).|count();",
                "a nested lambda's return is its own",
            ),
            (
                "l.stream().map(z -> { class L { Object f() { return n; } } return z; }).|count();",
                "a local class's return is its own",
            ),
        ] {
            assert_eq!(inferred(body), string(), "{why}");
        }
        for (body, why) in [
            (
                "l.stream().map(z -> { if (z == null) return n; return z; }).|count();",
                "two types have a least upper bound, not one type",
            ),
            (
                "l.stream().map(z -> { throw new IllegalStateException(); }).|count();",
                "no return",
            ),
            (
                "l.stream().map(z -> { return null; }).|count();",
                "only `null`",
            ),
            (
                "l.stream().map(z -> { return z.nothing(); }).|count();",
                "a result of unknown type",
            ),
            (
                "l.stream().map(z -> { if (z instanceof String n) { return n; } return n; }).|count();",
                "the second `n` is the parameter, which the pattern variable seems to hide",
            ),
        ] {
            let ty = inferred(body);
            assert!(
                !matches!(&ty, Type::External { arguments, .. } if arguments.iter().any(Type::is_known)),
                "{why}: {ty:?}"
            );
        }
    }

    fn collected(body: &str) -> Type {
        receiver_type_with(
            &format!(
                "import java.util.*; import java.util.stream.*; \
                 class A {{ void m(List<String> l, List raw, String s, List<Object> os) {{ {body} }} }}"
            ),
            &generic_world(),
        )
    }

    /// A generic call passed for a parameter is inferred toward it, for the
    /// variables its own arguments leave open (JLS 18.5.2.1).
    #[test]
    fn a_generic_call_is_inferred_toward_the_parameter_it_is_passed_for() {
        let of = |fqn: &str, element| generic_of(fqn, element);
        assert_eq!(
            collected("l.stream().collect(Collectors.toSet()).|size();"),
            of("java.util.Set", external_named(STRING)),
            "`? super T`'s lower bound"
        );
        assert_eq!(
            collected("l.stream().collect(Collectors.toList()).|size();"),
            of("java.util.List", external_named(STRING))
        );
        assert_eq!(
            collected("l.stream().collect(Collectors.joining()).|trim();"),
            external_named(STRING),
            "nothing open, as before"
        );
        assert_eq!(
            collected("l.stream().collect(Collectors.sample(s)).|size();"),
            of("java.util.List", external_named(STRING)),
            "a target that agrees with the argument keeps its binding"
        );
        assert_eq!(
            collected("l.stream().collect(Collectors.groupingBy(String::trim)).|size();"),
            Type::External {
                name: "Map".into(),
                fqn: "java.util.Map".into(),
                arguments: vec![
                    external_named(STRING),
                    of("java.util.List", external_named(STRING)),
                ],
            },
            "`K` from the reference, `T`, which it takes, from the target"
        );
        for (body, why) in [
            (
                "raw.stream().collect(Collectors.toSet()).|size();",
                "a raw receiver's `T` is nothing",
            ),
            (
                "l.stream().gather(Collectors.toSet()).|size();",
                "`? extends T` is an upper bound",
            ),
            (
                "l.stream().collect(Collectors.listing((Object o) -> s)).|size();",
                "a typed lambda settled `T` as `Object`, which the target does not undo",
            ),
            (
                "os.stream().collect(Collectors.sample(s)).|size();",
                "`String` from the argument and `Object` from the target are two lower bounds",
            ),
        ] {
            let ty = collected(body);
            assert!(
                !matches!(&ty, Type::External { arguments, .. } if arguments.iter().any(Type::is_known)),
                "{why}: {ty:?}"
            );
        }
    }

    /// Type the receiver marked `|` in a method of a class `A` beside a
    /// local `Box`, against [`generic_world`].
    fn referenced(body: &str) -> Type {
        receiver_type_with(
            &format!(
                "import java.util.*; \
                 class Box {{ Box(String s) {{}} String label(String x) {{ return x; }} \
                 String name() {{ return null; }} static String make(String x) {{ return x; }} \
                 static String none() {{ return null; }} String two(String a, String b) {{ return a; }} \
                 String over() {{ return null; }} String over(String a) {{ return a; }} \
                 String many(String... a) {{ return null; }} <T> String generic(T t) {{ return null; }} }} \
                 class A {{ void m(List<String> l, List<Box> boxes, List<Optional<String>> os, \
                 Box b) {{ {body} }} }}"
            ),
            &generic_world(),
        )
    }

    fn stream_of(element: Type) -> Type {
        generic_of("java.util.stream.Stream", element)
    }

    fn is_stream_of_box(ty: &Type) -> bool {
        matches!(ty, Type::External { fqn, arguments, .. }
            if fqn == "java.util.stream.Stream"
                && matches!(arguments.as_slice(), [Type::Declared { name, .. }] if name == "Box"))
    }

    /// A method reference binds what the interface returns as an expression
    /// lambda's body does: through a type, a value, a raw generic type's
    /// first parameter, and `::new`.
    #[test]
    fn a_method_reference_binds_what_the_interface_returns() {
        let string = || stream_of(external_named(STRING));
        assert_eq!(
            referenced("l.stream().map(String::trim).|count();"),
            string(),
            "an unbound receiver"
        );
        assert_eq!(
            referenced("boxes.stream().map(Box::name).|count();"),
            string()
        );
        assert_eq!(
            referenced("l.stream().map(Box::make).|count();"),
            string(),
            "a static method of the interface's arity"
        );
        assert_eq!(
            referenced("l.stream().map(b::label).|count();"),
            string(),
            "a bound receiver"
        );
        assert!(is_stream_of_box(&referenced(
            "l.stream().map(Box::new).|count();"
        )));
        assert_eq!(
            referenced("os.stream().map(Optional::get).|count();"),
            string(),
            "a raw generic type is read on the interface's first parameter"
        );
        assert_eq!(
            referenced("l.stream().map(String::trim).map(z -> z.|trim());"),
            external_named(STRING),
            "the next lambda's parameter is typed"
        );
        assert_eq!(
            referenced("Box Box = b; l.stream().map(Box::label).|count();"),
            string(),
            "a name a local binds is a value, not the type it shadows"
        );
    }

    /// Anything the form, the arity or the method's own shape leaves open
    /// binds nothing.
    #[test]
    fn a_method_reference_that_may_mean_another_method_binds_nothing() {
        for (body, why) in [
            (
                "boxes.stream().map(Box::label).|count();",
                "an instance method of n through a type",
            ),
            (
                "l.stream().map(Box::none).|count();",
                "a static method of n - 1",
            ),
            (
                "boxes.stream().map(b::name).|count();",
                "an instance method of n - 1 through a value",
            ),
            (
                "l.stream().map(b::make).|count();",
                "a static method through a value",
            ),
            ("l.stream().map(Box::two).|count();", "too many parameters"),
            (
                "l.stream().map(Box::over).|count();",
                "an overloaded method",
            ),
            ("l.stream().map(Box::many).|count();", "a variadic method"),
            ("l.stream().map(Box::generic).|count();", "a generic method"),
            ("l.stream().map(Box::missing).|count();", "no such method"),
            (
                "l.stream().map(List::new).|count();",
                "a generic type's constructor",
            ),
            (
                "os.stream().map(List::size).|count();",
                "a raw type the first parameter is not",
            ),
            (
                "class Q { String equals(String s) { return s; } } Q q = new Q(); \
                 os.stream().map(q::equals).|count();",
                "a name `Object` declares, whose `equals(Object)` Java picks here",
            ),
            (
                "class R extends Missing { String conv(Object o) { return null; } } \
                 R r = new R(); l.stream().map(r::conv).|count();",
                "a list that may be short by the method Java picks",
            ),
            (
                "class H<T> { <U> String pick() { return null; } } List<H<String>> hs = null; \
                 hs.stream().map(H::pick).|count();",
                "a generic method read on the interface's first parameter",
            ),
            (
                "List<List<String>> ls = null; ls.stream().map(List::first).|count();",
                "an external generic method, whose variables a raw owner erases",
            ),
        ] {
            assert!(
                !referenced(body).is_known()
                    || {
                        let ty = referenced(body);
                        matches!(&ty, Type::External { arguments, .. } if arguments.iter().all(|a| !a.is_known()))
                    },
                "{why}: {:?}",
                referenced(body)
            );
        }
    }

    /// A string literal is `java.lang.String` — not a file-local `String`,
    /// and nothing where the world does not hold the type.
    #[test]
    fn a_string_literal_is_a_java_lang_string() {
        let world = string_world(Vec::new());
        for source in [
            "class A { void m() { \"x\".|length(); } }",
            "class String {} class A { void m() { \"\"\"\n  x\"\"\".|length(); } }",
        ] {
            assert_eq!(
                receiver_type_with(source, &world),
                external_named(STRING),
                "{source}"
            );
        }
        assert!(!receiver_type("class A { void m() { \"x\".|length(); } }").is_known());
    }

    /// Two shapes that bind a variable to something other than what Java
    /// infers: a `? super T` is an upper bound, and a raw receiver erases an
    /// instance method's own variables (JLS 4.8).
    #[test]
    fn an_upper_bound_or_a_raw_receiver_binds_nothing() {
        for body in ["Collections.max(l, c).|hashCode();", "raw.pick(s).|trim();"] {
            assert!(!inferred(body).is_known(), "{body}");
        }
        assert_eq!(inferred("l.pick(s).|trim();"), external_named(STRING));
    }

    /// Every argument has its say, and javac takes the bound of all of them:
    /// arguments that disagree, one of unknown type, a block lambda, an array
    /// among several for a variadic tail, a typed lambda's written parameter,
    /// or nothing to bind from all leave the variable unknown.
    #[test]
    fn what_the_arguments_do_not_settle_stays_unknown() {
        for body in [
            "List.of(s, n).|stream();",
            "List.of(s, missing).|stream();",
            "List.of(s, a).|stream();",
            "Optional.of(null).|get();",
            "Objects.feed(s, (Object o) -> s).|get();",
            "Objects.either(s, z -> { return n; }).|get();",
            // `T` is what the lambda takes as well as returns, so javac
            // settles it before reading the body.
            "Objects.fold(x -> s).|get();",
        ] {
            let Type::External { arguments, .. } = inferred(body) else {
                panic!("{body}: the call still types its own class");
            };
            assert!(
                arguments.iter().all(|argument| !argument.is_known()),
                "{body}: {arguments:?}"
            );
        }
    }

    #[test]
    fn a_lambda_parameter_nothing_settles_is_unknown() {
        for body in [
            // A raw receiver binds nothing.
            "raw.forEach(x -> x.|length());",
            // `? extends E` is not the parameter's type.
            "l.loosely(x -> x.|length());",
            // Two candidates the lambda cannot separate.
            "l.twice(x -> x.|length());",
            // A lambda of the wrong arity is no `Consumer`.
            "l.forEach((x, y) -> x.|length());",
            // Not an argument of a call this records.
            "java.util.function.Consumer<String> c = x -> x.|length();",
        ] {
            assert!(!lambda_in(body).is_known(), "{body}: {:?}", lambda_in(body));
        }
    }

    /// A workspace interface that defaults an inherited JDK method and
    /// declares its own abstract one: the index stores no workspace member's
    /// abstractness, so the JDK's would look like the only one and type the
    /// parameter `Integer` where javac says `String`. Nothing is typed.
    #[test]
    fn a_functional_interface_holding_a_workspace_member_types_nothing() {
        let mut run = method("p.Attempt", "run", ExternalType::Void, vec![class(STRING)]);
        run.site = Some(crate::ExternalSite {
            doc_span: None,
            source: "/w/p/Attempt.java".into(),
            name_start: 0,
            name_end: 3,
            name_range: Some(javelin_document::Range::new(
                javelin_document::Position::default(),
                javelin_document::Position::default(),
            )),
        });
        let mut accept = method(
            "java.util.function.Consumer",
            "accept",
            ExternalType::Void,
            vec![class(INTEGER)],
        );
        accept.is_abstract = true;
        let world = string_world(Vec::new()).with("p.Attempt", &[], vec![run, accept]);
        let ty = receiver_type_with(
            "class A { void schedule(Attempt a) {} \
             void go() { schedule(label -> label.|toString()); } }",
            &world,
        );
        assert!(!ty.is_known(), "{ty:?}");
    }

    fn declared_name(ty: &Type) -> &str {
        match ty {
            Type::Declared { name, .. } => name,
            other => panic!("expected a declared type, got {other:?}"),
        }
    }

    #[test]
    fn a_local_takes_the_type_it_was_declared_with() {
        let ty = receiver_type("class Box {} class A { void m() { Box b; b.| } }");
        assert_eq!(declared_name(&ty), "Box");
    }

    #[test]
    fn a_var_local_takes_the_type_of_its_initialiser() {
        let ty = receiver_type("class Box {} class A { void m() { var b = new Box(); b.| ; } }");
        assert_eq!(declared_name(&ty), "Box");
    }

    #[test]
    fn a_var_local_initialised_from_a_call_takes_the_calls_return_type() {
        let source = "class Box {} \
                      class A { Box get() { return null; } void m() { var b = get(); b.| ; } }";
        assert_eq!(declared_name(&receiver_type(source)), "Box");
    }

    #[test]
    fn a_var_local_initialised_from_an_ambiguous_call_is_unknown() {
        // The `var` hop does not launder ambiguity: `sole_candidate` refuses
        // two zero-arity `get`s, so the initialiser has no type and neither
        // does the local. Refusing here is what stops the refusal turning
        // into a pick one level further out, where nothing would show it.
        //
        // Two no-argument methods of one name do not compile — which is the
        // point of choosing them. It is what a buffer looks like halfway
        // through a rename, no arity can separate them, and a resolver that
        // guesses is at its most confidently wrong exactly there.
        let ambiguous = "class X {} class Y {} \
                         class A { X get() { return null; } Y get() { return null; } \
                         void m() { var b = get(); b.| ; } }";
        assert!(!receiver_type(ambiguous).is_known());
        // Not vacuous: with one `get` the identical shape types the local, so
        // the unknown above is the ambiguity and not a `var` that never
        // carried its initialiser.
        let single = "class X {} \
                      class A { X get() { return null; } \
                      void m() { var b = get(); b.| ; } }";
        assert_eq!(declared_name(&receiver_type(single)), "X");
    }

    #[test]
    fn a_var_local_declared_inside_a_lambda_or_a_nested_block_still_infers() {
        // `FileSyntax` flattens locals to the member that encloses them, so
        // the `b` inside the `if` and the one inside the lambda body are both
        // reported as children of `m`. Inference never asks about blocks: it
        // reaches the declaration through `ScopeTree`'s binding and
        // `declaration_at`'s span lookup, and neither is block-shaped.
        let nested = "class Box {} \
                      class A { void m() { if (true) { var b = new Box(); b.| ; } } }";
        assert_eq!(declared_name(&receiver_type(nested)), "Box");
        let lambda = "class Box {} \
                      class A { void m() { Runnable r = () -> { var b = new Box(); b.| ; }; } }";
        assert_eq!(declared_name(&receiver_type(lambda)), "Box");
    }

    #[test]
    fn a_var_local_with_no_initialiser_is_unknown() {
        // `var b;` is not legal Java — the JLS requires an initialiser — so
        // this is a buffer mid-keystroke, which is the ordinary case. The
        // grammar still reads it as a `local_variable_declaration` whose
        // declarator has a `name` and no `value`, so the declaration is there
        // with `declared_type` `var` and nothing to infer from. Unknown is the
        // answer whichever way recovery goes: there is no initialiser to read.
        assert!(!receiver_type("class Box {} class A { void m() { var b; b.| ; } }").is_known());
    }

    #[test]
    fn a_var_local_initialised_with_a_lambda_is_unknown() {
        // A lambda's type is its target type, and a `var` gives it none — this
        // does not compile, and typing it would need a functional interface
        // this phase has no way to pick.
        assert!(!receiver_type("class A { void m() { var f = () -> 1; f.| ; } }").is_known());
    }

    #[test]
    fn a_var_local_initialised_with_a_method_reference_is_unknown() {
        let source = "class Box { static Box make() { return null; } } \
                      class A { void m() { var f = Box::make; f.| ; } }";
        assert!(!receiver_type(source).is_known());
    }

    #[test]
    fn a_var_in_a_for_each_header_over_an_array_is_its_component() {
        let source = "class Box {} class A { Box[] all; void m() { for (var b : all) { b.| ; } } }";
        assert!(matches!(receiver_type(source), Type::Declared { name, .. } if name == "Box"));
    }

    /// JLS 14.14.2: over an `Iterable<T>`, `T` — read as
    /// `iterator().next()`, so a raw iterable erases it as a raw call would.
    #[test]
    fn a_var_in_a_for_each_header_over_an_iterable_is_its_element() {
        const LIST: &str = "java.util.List";
        const ITERATOR: &str = "java.util.Iterator";
        const ITERABLE: &str = "java.lang.Iterable";
        const NAMES: &str = "a.Names";
        let element = || ExternalType::Variable("E".into());
        let iterator = |argument: Option<ExternalType>| match argument {
            Some(argument) => ExternalType::Parameterized(ITERATOR.into(), vec![argument]),
            None => class(ITERATOR),
        };
        // A world reports a closure's members in the asked type's variables,
        // `Iterable`'s `iterator()` among them, with the edge's argument bound.
        let world = string_world(Vec::new())
            .with(
                LIST,
                &[ITERABLE],
                vec![method(
                    ITERABLE,
                    "iterator",
                    iterator(Some(element())),
                    vec![],
                )],
            )
            .generic(&["E"])
            // `class Names implements Iterable { Iterator<String> iterator() }`:
            // raw, so Java's element is `Object` whatever the override says.
            .with(
                NAMES,
                &[ITERABLE],
                vec![
                    method(NAMES, "iterator", iterator(Some(class(STRING))), vec![]),
                    method(ITERABLE, "iterator", iterator(None), vec![]),
                ],
            )
            // `class Bag implements Iterable<Object> { It iterator() }`, with
            // `It`'s `next()` narrowed to a `String`: the element is `Object`.
            .with(
                "a.Bag",
                &[ITERABLE],
                vec![
                    method("a.Bag", "iterator", class("a.It"), vec![]),
                    method(ITERABLE, "iterator", iterator(Some(class(OBJECT))), vec![]),
                ],
            )
            .with(
                "a.It",
                &[ITERATOR],
                vec![method("a.It", "next", class(STRING), vec![])],
            )
            .with(ITERABLE, &[OBJECT], Vec::new())
            .with(
                ITERATOR,
                &[OBJECT],
                vec![method(ITERATOR, "next", element(), vec![])],
            )
            .generic(&["E"]);
        let over = |iterable: &str| {
            receiver_type_with(
                &format!(
                    "import a.Names; import a.Bag; \
                     class A {{ void m({iterable} all) {{ for (var s : all) {{ s.| ; }} }} }}"
                ),
                &world,
            )
        };
        assert_eq!(over("List<String>"), external_named(STRING));
        assert!(
            !over("List").is_known(),
            "a raw iterable's element is erased"
        );
        assert!(
            !over("Names").is_known(),
            "the `Iterable` it is, not what its own `iterator()` returns"
        );
        assert_eq!(over("Bag"), external_named(OBJECT), "nor what that returns");
    }

    #[test]
    fn a_class_literal_is_a_class_of_its_type() {
        const CLASS: &str = "java.lang.Class";
        let world = string_world(Vec::new())
            .with(CLASS, &[OBJECT], Vec::new())
            .generic(&["T"]);
        let literal = |written: &str, externals: &dyn Externals| {
            receiver_type_with(
                &format!(
                    "class Repo {{}} class A {{ void m() {{ var c = {written}.class; c.| ; }} }}"
                ),
                externals,
            )
        };
        assert!(matches!(
            literal("Repo", &world),
            Type::External { fqn, arguments, .. }
                if fqn == CLASS
                    && matches!(arguments.as_slice(), [Type::Declared { name, .. }] if name == "Repo")
        ));
        assert_eq!(
            literal("String", &world),
            generic_of(CLASS, external_named(STRING))
        );
        assert_eq!(
            literal("int", &world),
            generic_of(CLASS, external_named(INTEGER)),
            "a primitive's class is its box's"
        );
        assert!(
            !literal("Repo", &string_world(Vec::new())).is_known(),
            "a world with no `java.lang.Class`"
        );
    }

    /// `handle.attach(Repo.class)` binds `<T> T attach(Class<T>)`'s `T`.
    #[test]
    fn a_class_literal_argument_binds_a_generic_methods_variable() {
        const CLASS: &str = "java.lang.Class";
        const HANDLE: &str = "a.Handle";
        let own = || ExternalType::Unresolved("T".into());
        let mut attach = method(
            HANDLE,
            "attach",
            own(),
            vec![ExternalType::Parameterized(CLASS.into(), vec![own()])],
        );
        attach.type_parameters = vec!["T".to_owned()];
        let world = string_world(Vec::new())
            .with(CLASS, &[OBJECT], Vec::new())
            .generic(&["T"])
            .with(HANDLE, &[OBJECT], vec![attach]);
        let source = "import a.Handle; class A { void m(Handle h) { h.attach(String.class).| ; } }";
        assert_eq!(receiver_type_with(source, &world), external_named(STRING));
    }

    #[test]
    fn a_var_in_a_try_with_resources_is_unknown() {
        // Same ruling: a resource is not a declarator, and its `var` would need
        // `AutoCloseable` to mean anything.
        let source = "class Box {} \
                      class A { Box open() { return null; } \
                      void m() { try (var r = open()) { r.| ; } } }";
        assert!(!receiver_type(source).is_known());
    }

    #[test]
    fn a_self_referential_var_terminates_rather_than_recursing_forever() {
        // Liveness: `var a = a;` resolves its own initialiser back to itself,
        // so only the running depth stops it. It must return, not hang or
        // overflow the stack.
        assert!(!receiver_type("class A { void m() { var a = a; a.| ; } }").is_known());
        // Not vacuous: the local is in scope inside its own initialiser, so
        // the `a` on the right resolves back to the `a` on the left even when
        // a field of the same name stands behind it. Without the depth this
        // recurses forever rather than answering.
        let shadowing = "class Box {} class A { Box a; void m() { var a = a; a.| ; } }";
        assert!(!receiver_type(shadowing).is_known());
    }

    #[test]
    fn a_var_local_initialised_from_an_unresolvable_type_is_unknown() {
        // The open-buffer ceiling holds through inference: `StringBuilder` is
        // the JDK's, so the initialiser types as unknown and so does the local.
        assert!(
            !receiver_type("class A { void m() { var s = new StringBuilder(); s.| ; } }")
                .is_known()
        );
    }

    #[test]
    fn a_parameter_takes_its_declared_type() {
        let ty = receiver_type("class Box {} class A { void m(Box b) { b.| } }");
        assert_eq!(declared_name(&ty), "Box");
    }

    #[test]
    fn a_field_takes_its_declared_type() {
        let ty = receiver_type("class Box {} class A { Box b; void m() { b.| } }");
        assert_eq!(declared_name(&ty), "Box");
    }

    #[test]
    fn this_is_the_enclosing_type() {
        // A trailing `;` rather than a bare `this.` before the closing brace:
        // tree-sitter-java's error recovery for a dangling `this.` with
        // nothing else in the block collapses the whole enclosing class into
        // an `ERROR` node, so `FileSyntax` never sees a `class A` to be the
        // enclosing type of. A `;` gives the parser a statement to close, which
        // is also the shape a real editor buffer has almost always: the
        // written statement had a semicolon before the user reopened it.
        let ty = receiver_type("class A { void m() { this.| ; } }");
        assert_eq!(declared_name(&ty), "A");
    }

    #[test]
    fn super_is_the_enclosing_types_file_local_supertype() {
        let ty = receiver_type("class Base {} class A extends Base { void m() { super.| } }");
        assert_eq!(declared_name(&ty), "Base");
    }

    #[test]
    fn super_is_unknown_when_the_supertype_is_not_in_this_file() {
        let ty = receiver_type("class A extends ArrayList { void m() { super.| } }");
        assert!(!ty.is_known());
    }

    #[test]
    fn a_field_access_takes_the_fields_type() {
        // `this.o.inner.` rather than `o.inner.`: a dangling two-segment
        // chain of plain identifiers (`o.inner.`) is lexically ambiguous
        // between a field-access chain and a qualified type name, and
        // tree-sitter-java's recovery resolves that ambiguity toward a
        // `scoped_type_identifier` when nothing follows to say otherwise —
        // which projects as `Expr::Opaque` by design (see `expr.rs`'s own
        // `Map.Entry` note). Prefixing with `this.` breaks the ambiguity,
        // because `this` can never open a type name, while still exercising
        // the same `FieldAccess` chain through `Outer` to `Inner`.
        let source = "class Inner {} class Outer { Inner inner; } \
                      class A { Outer o; void m() { this.o.inner.| ; } }";
        assert_eq!(declared_name(&receiver_type(source)), "Inner");
    }

    #[test]
    fn a_call_takes_the_methods_return_type() {
        let source = "class Inner {} class Outer { Inner get() { return null; } } \
                      class A { Outer o; void m() { o.get().| } }";
        assert_eq!(declared_name(&receiver_type(source)), "Inner");
    }

    #[test]
    fn an_overloaded_call_is_unknown_rather_than_an_arbitrary_pick() {
        // Both overloads take one argument, so arity alone cannot tell them
        // apart — and neither can the argument, because `get(String)`'s
        // parameter is `java.lang.String`, which this phase cannot resolve.
        // An unresolvable name could be `Object`, `Number` or `Comparable`,
        // every one of which takes an `int`, so there is no exclusion to make.
        // Phase 3's classpath is what changes that; argument-type elimination
        // does separate this shape when both parameter types are file-local,
        // which `queries::definition_on_a_call_with_an_int_argument_picks_the_primitive_overload`
        // shows. (Overloads that differ in arity are not this case at all —
        // `o.get()` against `get()`/`get(int)` is unambiguous by count alone,
        // which is exactly what `member_type`'s arity filter is for; it is not
        // the "arbitrary pick" this test guards against.)
        let source = "class X {} class Y {} \
                      class O { X get(int i) { return null; } Y get(String s) { return null; } } \
                      class A { O o; void m() { o.get(1).| } }";
        assert!(!receiver_type(source).is_known());
    }

    #[test]
    fn a_varargs_method_still_needs_its_fixed_parameters() {
        // `m(String a, int... xs)` needs at least the `a`; calling with zero
        // arguments is not call-compatible, so accepting it as a candidate
        // would be as wrong as an arbitrary overload pick.
        let source = "class Box {} class O { Box m(String a, int... xs) { return null; } } \
                      class A { O o; void m2() { o.m().| ; } }";
        assert!(!receiver_type(source).is_known());
    }

    #[test]
    fn a_field_a_type_declares_itself_shadows_an_inherited_one() {
        let source = "class Box {} class Base { static Box X; } \
                      class Sub extends Base { Box X; } \
                      class A { Sub s; void m() { this.s.X.| ; } }";
        assert_eq!(declared_name(&receiver_type(source)), "Box");
    }

    #[test]
    fn a_nested_type_does_not_hide_an_inherited_field_of_its_name() {
        // Java keeps types out of the variable namespace (JLS 6.5.2), so
        // `Sub`'s `class g` hides nothing of `Base`'s `g`: a value position
        // means the field, a type position the class, and `Access::Value`
        // cannot tell which it is in.
        let source = "class Box {} class Base { Box g; } \
                      class Sub extends Base { class g {} } \
                      class A { Sub s; void m() { this.s.g.| ; } }";
        assert!(!receiver_type(source).is_known());
    }

    #[test]
    fn a_field_access_types_as_the_field_even_when_a_same_named_method_exists() {
        // 2.1's strictness: `Access::Value` sees only the variable namespace,
        // so `x.f` types as the field's `Marker` and never `Unknown` — even
        // though `member_definition_at`'s fallback would also accept the
        // method if there were no field at all. Loosening this filter would
        // turn an unambiguous access into a refusal.
        //
        // `this.x.f.` rather than `x.f.`: see `a_field_access_takes_the_fields_type`
        // for why a bare two-segment dangling chain recovers as a type name
        // instead of a field-access chain.
        let source = "class Marker {} \
                      class X { Marker f; Marker f() { return null; } } \
                      class A { X x; void m() { this.x.f.| ; } }";
        assert_eq!(declared_name(&receiver_type(source)), "Marker");
    }

    #[test]
    fn two_overloads_on_the_type_itself_are_still_ambiguous() {
        // The shadowing rule must not turn genuine ambiguity into a pick: both
        // candidates are declared on the receiver's own type, so there is
        // nothing to shadow, and the argument cannot separate them either —
        // `get(String)`'s parameter is the JDK's `String`, so no exclusion is
        // demonstrable. That is Phase 3's classpath rather than a gap here.
        let source = "class X {} class Y {} \
                      class O { X get(int i) { return null; } Y get(String s) { return null; } } \
                      class A { O o; void m() { o.get(1).| } }";
        assert!(!receiver_type(source).is_known());
    }

    #[test]
    fn a_call_the_arguments_separate_takes_the_chosen_overloads_return_type() {
        // What makes `f(new Foo()).` complete: elimination happens inside
        // `sole_candidate`, so the winner's return type is the receiver's type
        // and everything downstream of a `.` follows from it.
        let source = "class Foo {} class Bar {} class X {} class Y {} \
                      class A { X f(Foo p) { return null; } Y f(Bar p) { return null; } \
                      void m() { f(new Foo()).| ; } }";
        assert_eq!(declared_name(&receiver_type(source)), "X");
        let other = source.replace("new Foo()", "new Bar()");
        assert_eq!(declared_name(&receiver_type(&other)), "Y");
    }

    #[test]
    fn a_var_local_initialised_from_a_separable_overloaded_call_takes_that_overloads_type() {
        // The `var` hop carries a resolved overload exactly as it carries any
        // other initialiser type — the counterpart to
        // `a_var_local_initialised_from_an_ambiguous_call_is_unknown`, which
        // shows the refusal surviving the same hop.
        let source = "class Foo {} class Bar {} class X {} class Y {} \
                      class A { X f(Foo p) { return null; } Y f(Bar p) { return null; } \
                      void m() { var v = f(new Bar()); v.| ; } }";
        assert_eq!(declared_name(&receiver_type(source)), "Y");
    }

    #[test]
    fn a_nested_call_argument_deeper_than_the_cap_stops_instead_of_overflowing_the_stack() {
        // Built directly rather than through the parser, for the same reason
        // `a_receiver_chain_deeper_than_the_cap_stops_instead_of_overflowing_the_stack`
        // is: the parser's own `MAX_CHAIN` bounds what it projects, so a chain
        // that bypasses it is the gap the depth cap closes. This one nests
        // *arguments*, which is the mutual recursion argument-type elimination
        // introduces — typing an argument re-enters `sole_candidate`, which
        // types its arguments in turn.
        let source = "class A { int f(int i) { return i; } int f(A a) { return 0; } }";
        let document = TextDocument::new(DocumentId::new(FILE), 1, source);
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let scopes = ScopeTree::build(&document, &tree);
        // Inside `class A`, so `enclosing_type` finds it and both overloads of
        // `f` are candidates at every level: without that the walk stops at the
        // first level for want of a candidate and proves nothing.
        let at = source.find("int f").expect("the method") as u32;
        let span = Span {
            start_byte: at,
            end_byte: at,
            range: javelin_document::Range::new(
                javelin_document::Position::new(0, 0),
                javelin_document::Position::new(0, 0),
            ),
        };
        let nest = |levels: u32| {
            let mut expr = Expr::Literal {
                literal: Literal::Int,
                span,
            };
            for _ in 0..levels {
                expr = Expr::Call {
                    receiver: None,
                    name: "f".into(),
                    name_span: span,
                    arguments: vec![expr],
                    type_arguments: Vec::new(),
                };
            }
            expr
        };
        // Inside the cap the whole nest types through: the innermost argument
        // is an `int`, which excludes `f(A)`, so `f(int)` wins and returns the
        // `int` the level above it sees. Without this the assertion below
        // passes on a resolver that types nothing at all.
        assert_eq!(
            type_of(&nest(3), &set(&syntax, &scopes), &NoExternals),
            Type::Primitive(Primitive::Int)
        );
        // Past the cap it does not, and *which* bound cut it is the point: 70
        // levels spend 70 of `MAX_TYPED_ARGUMENTS`' 256, so the budget is not
        // what stopped this — the depth thread is. Delete `depth + 1` from
        // `candidate_at_depth`'s argument typing and this line fails while the
        // liveness check below still passes.
        assert!(!type_of(&nest(70), &set(&syntax, &scopes), &NoExternals).is_known());
        // Liveness at a depth no stack takes frame by frame. 1 000 rather than
        // its sibling's 10 000, and the difference is the test's own: dropping
        // a `Vec<Expr>` nested 10 000 deep overflows in the harness before
        // `type_of` is reached at all, which was measured with the call
        // removed.
        let _ = type_of(&nest(1_000), &set(&syntax, &scopes), &NoExternals);
    }

    #[test]
    fn a_chain_of_vars_over_an_overloaded_call_stops_instead_of_taking_exponential_time() {
        // The bound the depth cap does not give: each level types its two
        // arguments, and each of those re-types the level below, so the work
        // doubles per hop from a linear amount of source. At 40 levels that is
        // billions of typings within the depth cap alone; `MAX_TYPED_ARGUMENTS`
        // is what makes it finite.
        let mut source = String::from(
            "class A { int f(int i, int j) { return i; } int f(A a, A b) { return 0; } \
             void m() { var v0 = 1;",
        );
        for level in 1..=40 {
            source.push_str(&format!(
                " var v{level} = f(v{}, v{});",
                level - 1,
                level - 1
            ));
        }
        source.push_str(" v40.| ; } }");
        let started = std::time::Instant::now();
        // Liveness, and the answer is unknown: the budget runs out partway
        // down, so the deepest levels type as unknown and the ambiguity that
        // leaves propagates back up. Loose bound, and 40 levels unbounded is
        // not slow but non-terminating.
        assert!(!receiver_type(&source).is_known());
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "40 var levels took {elapsed:?}"
        );
    }

    #[test]
    fn an_unqualified_call_resolves_against_the_enclosing_type() {
        // Trailing `;`: see `this_is_the_enclosing_type` for why a dangling
        // call with nothing after it collapses the enclosing class.
        let source = "class Inner {} class A { Inner get() { return null; } \
                      void m() { get().| ; } }";
        assert_eq!(declared_name(&receiver_type(source)), "Inner");
    }

    /// JLS 15.12.1: an unqualified call searches the innermost enclosing type
    /// of which a method of that name is a member, so from a nested class it
    /// reaches the class around it.
    #[test]
    fn an_unqualified_call_in_a_nested_class_types_through_the_outer_classs_method() {
        let source = "class Box {} class Outer { Box get() { return null; } \
                      class Inner { void m() { get().| ; } } }";
        assert_eq!(declared_name(&receiver_type(source)), "Box");
    }

    #[test]
    fn a_new_expression_takes_the_type_it_creates() {
        let ty = receiver_type("class Box {} class A { void m() { new Box().| } }");
        assert_eq!(declared_name(&ty), "Box");
    }

    #[test]
    fn a_cast_takes_the_type_cast_to() {
        let ty = receiver_type("class Box {} class A { void m() { ((Box) o).| } }");
        assert_eq!(declared_name(&ty), "Box");
    }

    #[test]
    fn an_array_access_takes_the_element_type() {
        let ty = receiver_type("class Box {} class A { Box[] all; void m() { all[0].| } }");
        assert_eq!(declared_name(&ty), "Box");
    }

    #[test]
    fn an_opaque_expression_is_unknown() {
        assert!(!receiver_type("class A { void m() { (1 + 2).| } }").is_known());
    }

    #[test]
    fn a_receiver_naming_a_type_resolves_as_that_type() {
        let ty = receiver_type("class Box { static int C; } class A { void m() { Box.| } }");
        assert_eq!(declared_name(&ty), "Box");
    }

    #[test]
    fn a_receiver_chain_deeper_than_the_cap_stops_instead_of_overflowing_the_stack() {
        // Built directly rather than through `member_selection_at`: `Expr` is a
        // public enum with public variant fields, so a caller can construct a
        // chain the parser's own `MAX_CHAIN` never bounded. A chain built
        // through the parser was never in doubt; a chain built directly,
        // bypassing that cap entirely, is the gap `MAX_RECEIVER_DEPTH` closes.
        let span = Span {
            start_byte: 0,
            end_byte: 0,
            range: javelin_document::Range::new(
                javelin_document::Position::new(0, 0),
                javelin_document::Position::new(0, 0),
            ),
        };
        let mut expr = Expr::Name {
            name: "a".into(),
            span,
        };
        for _ in 0..10_000 {
            expr = Expr::FieldAccess {
                receiver: Box::new(expr),
                name: "b".into(),
                name_span: span,
            };
        }
        let document = TextDocument::new(DocumentId::new(FILE), 1, "class A {}");
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let scopes = ScopeTree::build(&document, &tree);
        // Liveness: this must return rather than hang or overflow the stack.
        assert!(!type_of(&expr, &set(&syntax, &scopes), &NoExternals).is_known());
    }

    // ---- Tier 3, slice 1: what a type hint may claim ------------------------

    /// The hint the `var` called `v` in `source` would carry.
    fn var_hint(source: &str, externals: &dyn Externals) -> Option<HintedType> {
        fn find<'d>(decls: &'d [Decl], name: &str) -> Option<&'d Decl> {
            decls.iter().find_map(|decl| {
                (decl.kind == DeclKind::LocalVariable && decl.name == name)
                    .then_some(decl)
                    .or_else(|| find(&decl.children, name))
            })
        }
        let document = TextDocument::new(DocumentId::new(FILE), 1, source.to_owned());
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let scopes = ScopeTree::build(&document, &tree);
        let decl = find(&syntax.declarations, "v").expect("a local v");
        hinted_type(
            decl.initializer.as_deref().expect("a var"),
            &set(&syntax, &scopes),
            externals,
        )
    }

    fn var_label(source: &str, externals: &dyn Externals) -> Option<String> {
        var_hint(source, externals).map(|hint| hint.label)
    }

    /// The hint the lambda parameter at `|` would carry.
    fn lambda_hint(marked: &str, externals: &dyn Externals) -> Option<String> {
        let offset = marked.find('|').expect("mark the parameter with |") as u32;
        let source = marked.replace('|', "");
        let document = TextDocument::new(DocumentId::new(FILE), 1, source);
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let scopes = ScopeTree::build(&document, &tree);
        let lambda = syntax.lambda_parameter(offset).expect("a lambda parameter");
        lambda_parameter_hint(lambda, &set(&syntax, &scopes), externals).map(|hint| hint.label)
    }

    fn trimmed_world(complete: bool) -> World {
        let trim = method(STRING, "trim", class(STRING), vec![]);
        let to_string = method(STRING, "toString", class(STRING), vec![]);
        let world = World::holding(&[OBJECT]).with(INTEGER, &[OBJECT], Vec::new());
        if complete {
            world.with(STRING, &[OBJECT], vec![trim, to_string])
        } else {
            world.with_partial(STRING, &[OBJECT], vec![trim, to_string])
        }
    }

    const TRIMMED: &str = "class A { void m(String s) { var v = s.trim(); } }";

    #[test]
    fn a_var_is_hinted_with_its_initialisers_type_and_the_names_it_read() {
        let hint = var_hint(TRIMMED, &trimmed_world(true)).expect("a hint");
        assert_eq!(hint.label, "String");
        assert!(hint.written.contains(&"String".to_owned()), "{hint:?}");
    }

    /// A primitive read on the way is a keyword, which no gate need doubt.
    #[test]
    fn a_primitive_is_hinted_and_reads_no_name() {
        let source = "class A { void m(int local) { var v = local; } }";
        let hint = var_hint(source, &trimmed_world(true)).expect("a hint");
        assert_eq!(
            (hint.label.as_str(), hint.written),
            ("int", Vec::<String>::new())
        );
    }

    /// A list that may be short may lack the overload Java picks.
    #[test]
    fn a_member_picked_from_a_list_that_may_be_short_is_not_hinted() {
        assert_eq!(var_label(TRIMMED, &trimmed_world(false)), None);
        assert!(
            type_of_var(TRIMMED, &trimmed_world(false)).is_known(),
            "the premise: definition still answers"
        );
    }

    fn type_of_var(source: &str, externals: &dyn Externals) -> Type {
        let marked = source.replace("var v = s.trim()", "var v = s.trim(); v.|x()");
        receiver_type_with(&marked, externals)
    }

    #[test]
    fn a_method_object_declares_is_not_hinted() {
        let source = "class A { void m(String s) { var v = s.toString(); } }";
        assert_eq!(var_label(source, &trimmed_world(true)), None);
    }

    /// A workspace member's written type is read in its own file, and only a
    /// file that settles it is believed.
    #[test]
    fn a_workspace_member_whose_type_its_file_may_not_settle_is_not_hinted() {
        let fetched = |certain| {
            let mut found = method("a.Repo", "find", class(STRING), vec![]);
            found.certain = certain;
            found.site = Some(crate::ExternalSite {
                source: "/w/Repo.java".into(),
                name_start: 0,
                name_end: 4,
                name_range: Some(javelin_document::Range::new(
                    javelin_document::Position::default(),
                    javelin_document::Position::default(),
                )),
                doc_span: None,
            });
            string_world(Vec::new()).with("a.Repo", &[OBJECT], vec![found])
        };
        let source = "class A { void m(a.Repo r) { var v = r.find(); } }";
        assert_eq!(var_label(source, &fetched(true)).as_deref(), Some("String"));
        assert_eq!(var_label(source, &fetched(false)), None);
    }

    /// A raw type may be an erasure's guess; a type variable is no type a
    /// reader can look up.
    #[test]
    fn a_raw_type_or_a_type_variable_is_not_hinted() {
        let world = string_world(Vec::new())
            .with("java.util.List", &[OBJECT], Vec::new())
            .generic(&["E"]);
        let typed = "import java.util.List; class A { void m(List<String> l) { var v = l; } }";
        assert_eq!(var_label(typed, &world).as_deref(), Some("List<String>"));
        let raw = "import java.util.List; class A { void m(List l) { var v = l; } }";
        assert_eq!(var_label(raw, &world), None);
        let variable = "class A<T> { void m(T t) { var v = t; } }";
        assert_eq!(var_label(variable, &world), None);
        let array = "class A { void m(String[] a) { var v = a; } }";
        assert_eq!(var_label(array, &world).as_deref(), Some("String[]"));
    }

    /// A field read from a nested type may be one the nested type inherits.
    #[test]
    fn an_outer_field_read_from_a_nested_type_is_not_hinted() {
        let source = "class A { String f; class B { void m() { var v = f; } } }";
        assert_eq!(var_label(source, &trimmed_world(true)), None);
        let own = "class A { String f; void m() { var v = f; } }";
        assert_eq!(
            var_label(own, &trimmed_world(true)).as_deref(),
            Some("String")
        );
    }

    #[test]
    fn a_generic_methods_inferred_result_is_hinted() {
        let source = "import java.util.*; class A { void m(String s) { var v = Optional.of(s); } }";
        assert_eq!(
            var_label(source, &generic_world()).as_deref(),
            Some("Optional<String>")
        );
    }

    #[test]
    fn an_implicit_lambda_parameter_is_hinted_where_its_call_was_picked_from_a_whole_list() {
        let source = "import java.util.List; class A { void m(List<String> l) { l.forEach(|x -> x.length()); } }";
        assert_eq!(
            lambda_hint(source, &lambda_world()).as_deref(),
            Some("String")
        );
        let typed = "import java.util.List; class A { void m(List<String> l) { l.forEach((String |x) -> x.length()); } }";
        assert_eq!(
            lambda_hint(typed, &lambda_world()),
            None,
            "a written type is no hint"
        );
    }

    #[test]
    fn a_lambda_whose_call_or_interface_may_be_short_is_not_hinted() {
        const LIST: &str = "java.util.List";
        const CONSUMER: &str = "java.util.function.Consumer";
        let world = |list_whole: bool, consumer_whole: bool| {
            let mut accept = method(
                CONSUMER,
                "accept",
                ExternalType::Void,
                vec![ExternalType::Variable("T".into())],
            );
            accept.is_abstract = true;
            let for_each = method(
                LIST,
                "forEach",
                ExternalType::Void,
                vec![ExternalType::Parameterized(
                    CONSUMER.into(),
                    vec![ExternalType::Variable("E".into())],
                )],
            );
            let world = string_world(Vec::new());
            let world = if list_whole {
                world.with(LIST, &[OBJECT], vec![for_each])
            } else {
                world.with_partial(LIST, &[OBJECT], vec![for_each])
            }
            .generic(&["E"]);
            if consumer_whole {
                world.with(CONSUMER, &[OBJECT], vec![accept])
            } else {
                world.with_partial(CONSUMER, &[OBJECT], vec![accept])
            }
            .generic(&["T"])
        };
        let source = "import java.util.List; class A { void m(List<String> l) { l.forEach(|x -> x.length()); } }";
        assert_eq!(
            lambda_hint(source, &world(true, true)).as_deref(),
            Some("String")
        );
        assert_eq!(lambda_hint(source, &world(false, true)), None);
        assert_eq!(lambda_hint(source, &world(true, false)), None);
    }

    // ---- the review's wrong hints ------------------------------------------

    /// An anonymous class's supertype is not walked, so its own list is short
    /// by every member it inherits: `f(String)` is Java's pick here.
    #[test]
    fn a_pick_inside_an_anonymous_class_is_not_hinted() {
        let source = "class R1 {} class R2 {} class Base { R1 f(String s) { return null; } } \
                      class A { void m() { new Base() { R2 f(Object o) { return null; } \
                      void g() { var v = f(\"x\"); } }; } }";
        assert_eq!(var_label(source, &trimmed_world(true)), None);
    }

    /// A variable holding an anonymous instance has its members, a covariant
    /// override among them, which its written supertype's list lacks.
    #[test]
    fn a_member_of_an_anonymous_instance_is_not_hinted() {
        let source = "class Base { Object get() { return null; } } class A { void m() { \
                      var r = new Base() { String get() { return null; } }; var v = r.get(); } }";
        assert_eq!(var_label(source, &trimmed_world(true)), None);
        let named = source.replace("new Base() { String get() { return null; } }", "new Base()");
        assert_eq!(
            var_label(&named, &trimmed_world(true)).as_deref(),
            Some("Object"),
            "the premise: a named class's member is hinted"
        );
    }

    /// `Optional.<Object>of(s)` is an `Optional<Object>`, as written, and not
    /// the `Optional<String>` binding the argument would say (JLS 15.12.2.1).
    #[test]
    fn a_call_with_explicit_type_arguments_is_hinted_as_written() {
        let label = |call: &str| {
            let source =
                format!("import java.util.*; class A {{ void m(String s) {{ var v = {call}; }} }}");
            var_label(&source, &generic_world())
        };
        assert_eq!(
            label("Optional.<Object>of(s)").as_deref(),
            Some("Optional<Object>")
        );
        assert_eq!(
            label("Optional.<Object, String>of(s)"),
            None,
            "a count that is not the method's says nothing"
        );
        assert_eq!(
            label("Optional.<Missing>of(s)"),
            None,
            "a type argument nothing resolves binds nothing"
        );
        assert_eq!(
            label("Optional.<int>of(s)"),
            None,
            "a primitive is no type argument"
        );
    }

    /// `Foo.make()` reads `Foo` as an inherited field before a type (JLS
    /// 6.5.2), so `make` is `Bar`'s.
    #[test]
    fn a_type_name_an_inherited_field_means_is_read_as_the_field() {
        let source = "class Bar { String make() { return null; } } \
                      class Foo { static Integer make() { return null; } } \
                      class Shade { Bar Foo; } \
                      class A extends Shade { void m() { var v = Foo.make(); } }";
        assert_eq!(
            var_label(source, &trimmed_world(true)).as_deref(),
            Some("String")
        );
        let unshaded = source.replace("Bar Foo;", "");
        assert_eq!(
            var_label(&unshaded, &trimmed_world(true)).as_deref(),
            Some("Integer"),
            "the premise: with no field the type is read"
        );
    }

    /// A raw argument makes the call unchecked (JLS 15.12.2.6), and Java
    /// erases its result.
    #[test]
    fn a_call_with_a_raw_argument_is_not_hinted() {
        let source = "import java.util.*; class A { List<String> get(Map<String, String> m) { return null; } \
                      void m(Map raw) { var v = get(raw); } }";
        assert_eq!(var_label(source, &lambda_world()), None);
        let typed = source.replace("Map raw", "Map<String, String> raw");
        assert_eq!(
            var_label(&typed, &lambda_world()).as_deref(),
            Some("List<String>"),
            "the premise: a parameterized argument is hinted"
        );
    }

    /// A method of this file's own is read through every type it writes,
    /// its result's as well as its parameters', and a primitive is none.
    #[test]
    fn a_local_methods_written_types_are_read_but_not_its_primitives() {
        let source = "class Foo {} class A { int size(Foo f) { return 0; } \
                      void m(Foo g) { var v = size(g); } }";
        let hint = var_hint(source, &trimmed_world(true)).expect("a hint");
        assert_eq!(hint.label, "int");
        assert_eq!(hint.written, vec!["Foo".to_owned(), "Foo".to_owned()]);
    }

    /// The functional interface a lambda parameter is read through is a name
    /// it was read through.
    #[test]
    fn a_lambda_parameter_reads_its_functional_interfaces_name() {
        let marked = "import java.util.List; class A { void m(List<String> l) { l.forEach(|x -> x.length()); } }";
        let offset = marked.find('|').expect("marked") as u32;
        let source = marked.replace('|', "");
        let document = TextDocument::new(DocumentId::new(FILE), 1, source);
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let scopes = ScopeTree::build(&document, &tree);
        let lambda = syntax.lambda_parameter(offset).expect("a lambda parameter");
        let hint =
            lambda_parameter_hint(lambda, &set(&syntax, &scopes), &lambda_world()).expect("a hint");
        assert!(hint.written.contains(&"Consumer".to_owned()), "{hint:?}");
    }

    /// What typeDefinition answers at the marked offset.
    fn type_target(marked: &str, externals: &dyn Externals) -> Option<TypeTarget> {
        let offset = marked.find('|').expect("mark the cursor with |") as u32;
        let source = marked.replace('|', "");
        let document = TextDocument::new(DocumentId::new(FILE), 1, source);
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let scopes = ScopeTree::build(&document, &tree);
        type_definition_at(&document, &tree, &set(&syntax, &scopes), externals, offset)
            .map(|found| found.target)
    }

    fn string_target() -> Option<TypeTarget> {
        Some(TypeTarget::External(STRING.to_owned()))
    }

    #[test]
    fn a_name_a_call_and_a_field_access_target_their_values_type() {
        let world = trimmed_world(true);
        for marked in [
            "class A { void m(String s) { var v = s|; } }",
            "class A { void m(String s) { var v = s.tri|m(); } }",
            "class B { String f; } class A { void m(B b) { var v = b.f|; } }",
        ] {
            assert_eq!(type_target(marked, &world), string_target(), "{marked}");
        }
    }

    #[test]
    fn a_variables_own_name_targets_its_type() {
        let world = trimmed_world(true);
        for marked in [
            "class A { void m(String s|) {} }",
            "class A { void m(String s) { String x| = s; } }",
            "class A { void m(String s) { var v| = s.trim(); } }",
            "class A { String f|; }",
            "class A { void m(String[] all) { for (String e| : all) {} } }",
        ] {
            assert_eq!(type_target(marked, &world), string_target(), "{marked}");
        }
        let lambda =
            "import java.util.List; class A { void m(List<String> l) { l.forEach(x| -> {}); } }";
        assert_eq!(type_target(lambda, &lambda_world()), string_target());
        let pair = "import java.util.Map; \
                    class A { void m(Map<String, Integer> counts) { counts.forEach((k|, n) -> {}); } }";
        assert_eq!(type_target(pair, &lambda_world()), string_target());
    }

    #[test]
    fn a_type_this_file_declares_is_targeted_at_its_name() {
        let source = "class B {} class A { void m(B b|) {} }";
        let at = source.find("B {").expect("B's name") as u32;
        let Some(TypeTarget::Declared(site)) = type_target(source, &trimmed_world(true)) else {
            panic!("a declared target");
        };
        assert_eq!(site.span.start_byte, at);
    }

    #[test]
    fn an_array_targets_its_element_and_a_parameterized_type_its_raw_type() {
        assert_eq!(
            type_target("class A { void m(String[][] a|) {} }", &trimmed_world(true)),
            string_target()
        );
        for marked in [
            "import java.util.List; class A { void m(List<String> l|) {} }",
            "import java.util.List; class A { void m(List raw|) {} }",
        ] {
            assert_eq!(
                type_target(marked, &lambda_world()),
                Some(TypeTarget::External("java.util.List".to_owned())),
                "{marked}"
            );
        }
    }

    #[test]
    fn a_primitive_a_variable_and_a_name_with_no_value_target_nothing() {
        let world = trimmed_world(true);
        for marked in [
            "class A { void m(int i|) {} }",
            "class A<T> { void m(T t|) {} }",
            "class A { void m|() {} }",
            "class A| {}",
            "class A { void m(Strin|g s) {} }",
        ] {
            assert_eq!(type_target(marked, &world), None, "{marked}");
        }
    }

    /// The same doubt a hint has: a pick from a list that may be short.
    #[test]
    fn a_member_picked_from_a_list_that_may_be_short_targets_nothing() {
        let marked = "class A { void m(String s) { var v = s.tri|m(); } }";
        assert_eq!(type_target(marked, &trimmed_world(false)), None);
        assert_eq!(type_target(marked, &trimmed_world(true)), string_target());
    }

    /// Inside a local class a member type its supertypes declare may be what
    /// a written name means, and no closure the server walks reaches it.
    #[test]
    fn a_type_written_inside_a_local_class_targets_nothing() {
        let world = trimmed_world(true);
        let local = "class Item {} class A { void m() { class L { Item item; } \
                     L l = new L(); var x = l.it|em; } }";
        assert_eq!(type_target(local, &world), None);
        let member =
            "class Item {} class A { class L { Item item; } void m(L l) { var x = l.it|em; } }";
        assert!(
            matches!(type_target(member, &world), Some(TypeTarget::Declared(_))),
            "the premise: a member class's field answers"
        );
        let inside =
            "class Item {} class A { void m() { class L { void k(Item i) { var x = i|; } } } }";
        assert_eq!(type_target(inside, &world), None);
        let nested = "class Item {} class A { class L { void k(Item i) { var x = i|; } } }";
        assert!(matches!(
            type_target(nested, &world),
            Some(TypeTarget::Declared(_))
        ));
    }

    /// `s` after the `if` is the field: the pattern variable's scope ended.
    #[test]
    fn a_pattern_variable_that_may_have_left_scope_targets_nothing() {
        let world = trimmed_world(true);
        let after = "class B {} class A { B s; void m(Object o) { \
                     if (o instanceof String s) { } var x = s|; } }";
        assert_eq!(type_target(after, &world), None);
        let alone = "class A { void m(Object o) { if (o instanceof String s) { var x = s|; } } }";
        assert_eq!(type_target(alone, &world), string_target(), "the premise");
    }

    /// A variable obscures a type of the same name (JLS 6.4.2).
    #[test]
    fn a_variable_nothing_types_is_not_read_as_a_type() {
        let source = "class Order {} record P(Object a) {} \
                      class A { void m(Object o) { if (o instanceof P(Object Order)) { var x = Ord|er; } } }";
        assert_eq!(type_target(source, &trimmed_world(true)), None);
    }
}
