//! A grammar-free view of the expression a member is being selected from.
//!
//! `javelin-semantic` types receivers, and typing one means walking an
//! expression. Walking tree-sitter nodes is grammar knowledge, and this crate
//! is the only one that has it — so expressions are projected here exactly as
//! declarations are projected in [`crate::syntax`], and semantic interprets the
//! projection.
//!
//! [`Expr`] is deliberately a closed list. Every Java expression form that is
//! not a variant becomes [`Expr::Opaque`], whose type is unknown, which is the
//! honest answer and today's behaviour.

use javelin_document::TextDocument;
use tree_sitter::Node;

use crate::SyntaxTree;
use crate::queries::span as span_of;
use crate::syntax::Span;

/// How deep a receiver chain is followed. A generated file can nest builder
/// calls arbitrarily; a request must still terminate.
const MAX_CHAIN: u32 = 64;

/// One expression, projected far enough to type it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// A bare identifier: a local, a parameter, a field, or a type name.
    Name {
        name: String,
        span: Span,
    },
    This {
        span: Span,
    },
    Super {
        span: Span,
    },
    /// `receiver.name`
    FieldAccess {
        receiver: Box<Expr>,
        name: String,
        name_span: Span,
    },
    /// `receiver.name(…)`, or `name(…)` with no receiver.
    Call {
        receiver: Option<Box<Expr>>,
        name: String,
        name_span: Span,
        arguments: Vec<Expr>,
        /// `receiver.<A, B>name(…)`'s `A` and `B`, as written; empty where
        /// the call writes none.
        type_arguments: Vec<String>,
    },
    /// `new Type(…)`. The type is as written.
    New {
        type_text: String,
        /// `new Type(…) { … }`: the value is an anonymous subclass, whose own
        /// members are callable on it although `Type` declares none of them.
        anonymous: bool,
        /// What the constructor is passed, which selects it.
        arguments: Vec<Expr>,
        span: Span,
    },
    /// `array[index]`
    ArrayAccess {
        array: Box<Expr>,
        span: Span,
    },
    /// `(Type) inner`. The type is as written.
    Cast {
        type_text: String,
        inner: Box<Expr>,
        span: Span,
    },
    /// `x -> body` or `(a, b) -> { … }`: what types the lambda's result. An
    /// expression body is one result; a block's are the values of the
    /// `return`s that are its own, and `None` when it has none, a `return;`,
    /// or too many to read. `typed` when its parameters are written with
    /// types, `(String s) -> …`, which makes them say something an implicit
    /// lambda's do not.
    Lambda {
        results: Option<Vec<Expr>>,
        typed: bool,
        span: Span,
    },
    /// `target::name`, or `target::new` with no `name`. `type_text` is set
    /// where the grammar says the left is a type (`List<String>`, `int[]`);
    /// otherwise the left is an ambiguous name or an expression, projected
    /// as `target`, and which it is is a question of scope (JLS 6.5.2).
    MethodRef {
        target: Box<Expr>,
        type_text: Option<String>,
        name: Option<String>,
        span: Span,
    },
    /// A literal, which is the one expression form whose type needs no lookup.
    Literal {
        literal: Literal,
        span: Span,
    },
    /// `Type.class`. The type is as written.
    ClassLiteral {
        type_text: String,
        span: Span,
    },
    /// The element a for-each over `iterable` binds: never written as an
    /// expression, only stood in for a `for (var x : iterable)`'s initialiser.
    Element {
        iterable: Box<Expr>,
        span: Span,
    },
    /// Every form the list above does not cover.
    Opaque {
        span: Span,
    },
}

/// The kind of a literal expression. Distinguishing `int` from `long`, or
/// `float` from `double`, is a suffix on the source text, not a grammar node
/// of its own — see `project`'s literal arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Literal {
    Int,
    Long,
    Float,
    Double,
    Char,
    Boolean,
    String,
    Null,
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Name { span, .. }
            | Expr::This { span }
            | Expr::Super { span }
            | Expr::New { span, .. }
            | Expr::ArrayAccess { span, .. }
            | Expr::Cast { span, .. }
            | Expr::Literal { span, .. }
            | Expr::Lambda { span, .. }
            | Expr::MethodRef { span, .. }
            | Expr::ClassLiteral { span, .. }
            | Expr::Element { span, .. }
            | Expr::Opaque { span } => *span,
            Expr::FieldAccess { name_span, .. } | Expr::Call { name_span, .. } => *name_span,
        }
    }
}

/// A member selection in progress: `receiver.name`, with the cursor somewhere
/// inside `name`.
///
/// `prefix` and `name` are different questions and both callers need one of
/// them. Completing at `b.wid|th` filters on `wid`; going to definition at the
/// same offset must find `width`. Carrying only what precedes the cursor would
/// make definition search for a name that does not exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub receiver: Expr,
    /// What has been typed before the cursor. Empty right after the separator.
    pub prefix: String,
    /// The whole identifier the cursor sits in, cursor to either side of it.
    /// Equal to `prefix` when the cursor is at the identifier's end.
    pub name: String,
    /// The span of `name`, when there is one.
    pub name_span: Option<Span>,
    /// How `name` is being used: a value, a method reference, or a call.
    pub kind: SelectionKind,
}

/// How the selected name is being used, which decides which namespace and
/// which arity can answer it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionKind {
    /// `receiver.name` with no argument list: a field, an enum constant, a
    /// nested type — or a method the user has not finished calling.
    Value,
    /// `receiver::name`: a method, with no argument list to filter on.
    MethodRef,
    /// `receiver.name(…)`: a method, filtered by arity and argument type.
    Call { arguments: Vec<Expr> },
}

/// The member selection the cursor sits in, if it sits in one.
///
/// `None` covers every honest miss: the cursor is not after a `.` or a `::`,
/// the offset is off a character boundary, or the receiver could not be found.
pub fn member_selection_at(
    document: &TextDocument,
    tree: &SyntaxTree,
    offset: u32,
) -> Option<Selection> {
    let offset = offset.min(document.text().len() as u32);
    let text = document.text().get(..offset as usize)?;

    // The identifier being typed, and what stands to its left.
    let start = text
        .char_indices()
        .rev()
        .find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '$'))
        .map_or(0, |(index, c)| index + c.len_utf8());
    let (left, prefix) = text.split_at(start);
    let left = left.trim_end();

    // `int... args` opens a parameter; it does not select from anything.
    if left.ends_with("...") {
        return None;
    }
    let separator = if left.ends_with("::") {
        2
    } else if left.ends_with('.') {
        1
    } else {
        return None;
    };

    // Trimmed a second time, after the separator is cut off: `left` was
    // trimmed before the `.` was identified, so `left.len() - separator` is
    // where the `.` sits, not where the receiver ends. With whitespace between
    // the two — `b . w`, or a leading-dot chain across lines — that offset
    // lands the descendant lookup on the whitespace, and tree-sitter answers
    // with the enclosing node, which is the very selection being completed.
    let receiver_end = left[..left.len() - separator].trim_end().len() as u32;
    let node = tree.root().descendant_for_byte_range(
        receiver_end.saturating_sub(1) as usize,
        receiver_end as usize,
    )?;
    let receiver = project(
        document,
        largest_expression_ending_at(node, receiver_end),
        0,
    );

    // The rest of the identifier, which the cursor may be sitting inside.
    let after = document.text().get(offset as usize..).unwrap_or("");
    let tail = after
        .char_indices()
        .find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '$'))
        .map_or(after.len(), |(index, _)| index);
    let name = format!("{prefix}{}", &after[..tail]);

    let name_span = (!name.is_empty()).then(|| {
        let start = start as u32;
        let end = offset + tail as u32;
        Span {
            start_byte: start,
            end_byte: end,
            range: javelin_document::Range::new(
                document.position_at(start),
                document.position_at(end),
            ),
        }
    });

    // `::` never carries an argument list. For `.`, the selected name is a
    // call only when it is itself the `name` field of a `method_invocation` —
    // `(` sits to the right of the cursor, so the tree answers this, not the
    // text to the left that the rest of this function reads.
    let kind = if separator == 2 {
        SelectionKind::MethodRef
    } else {
        let invocation = crate::queries::identifier_at(tree.root(), offset).and_then(|node| {
            let parent = node.parent()?;
            (parent.kind() == "method_invocation"
                && parent.child_by_field_name("name") == Some(node))
            .then_some(parent)
        });
        match invocation {
            Some(invocation) => SelectionKind::Call {
                arguments: project_arguments(document, invocation, 1),
            },
            None => SelectionKind::Value,
        }
    };

    Some(Selection {
        receiver,
        prefix: prefix.to_owned(),
        name,
        name_span,
        kind,
    })
}

/// The unqualified call the cursor's name belongs to, if it belongs to one.
///
/// `Some` only for `name(…)` with no receiver — a call *with* one is
/// `member_selection_at`'s question and answers `None` here, so the two never
/// both claim an offset. The projection runs through `project`, so the arity
/// and the spans come from the same place they do everywhere else.
pub fn unqualified_call_at(
    document: &TextDocument,
    tree: &SyntaxTree,
    offset: u32,
) -> Option<Expr> {
    // `identifier_at`, not a fresh lookup: "a cursor immediately after a name
    // is still on it" is where the cursor lands after typing the name, and
    // definition must not depend on which of the two queries answered.
    let name = crate::queries::identifier_at(tree.root(), offset)?;
    let call = name.parent()?;
    if call.kind() != "method_invocation"
        || call.child_by_field_name("name") != Some(name)
        || call.child_by_field_name("object").is_some()
    {
        return None;
    }
    match project(document, call, 0) {
        found @ Expr::Call { receiver: None, .. } => Some(found),
        _ => None,
    }
}

/// The value the identifier at an offset names, as the expression whose type
/// is that value's: a call at its name, a field access at its right-hand
/// side, and a name in expression position or a variable's own declaring
/// name as itself, which the scope tree binds to the declaration.
///
/// `None` for anything else — a type's or a method's own name, a label, a
/// key, `this` — since none of them has a value.
pub fn value_at(document: &TextDocument, tree: &SyntaxTree, offset: u32) -> Option<Expr> {
    let name = crate::queries::identifier_at(tree.root(), offset)?;
    let parent = name.parent()?;
    let is = |field: &str| parent.child_by_field_name(field) == Some(name);
    let declares = match parent.kind() {
        "method_invocation" if is("name") => return Some(project(document, parent, 0)),
        "field_access" if is("field") => return Some(project(document, parent, 0)),
        "variable_declarator"
        | "formal_parameter"
        | "catch_formal_parameter"
        | "resource"
        | "enhanced_for_statement" => is("name"),
        "lambda_expression" => is("parameters"),
        "inferred_parameters" => true,
        _ => false,
    };
    (declares || crate::queries::is_expression_name(name)).then(|| project(document, name, 0))
}

/// The `case` constant whose simple name is at `offset` — `MON` in `case MON
/// ->` or `case TUE, MON:` — with its span and the switch's selector, whose
/// type decides what the name means: under an enum selector it is that enum's
/// constant whatever the scope binds (JLS 14.11.1). `None` on a qualified
/// constant, a pattern, a guard and anything that is not a `case` label.
pub fn case_constant_at(
    document: &TextDocument,
    tree: &SyntaxTree,
    offset: u32,
) -> Option<(String, Span, Expr)> {
    let name = crate::queries::identifier_at(tree.root(), offset)?;
    let selector = case_selector(name)?;
    Some((
        crate::queries::text(document, name)?.to_owned(),
        span_of(document, name),
        project(document, selector, 0),
    ))
}

/// The selector of the switch whose `case` label `name` is a constant of.
pub(crate) fn case_selector(name: Node<'_>) -> Option<Node<'_>> {
    let label = name.parent()?;
    if name.kind() != "identifier" || label.kind() != "switch_label" {
        return None;
    }
    let switch = label.parent()?.parent()?.parent()?;
    if switch.kind() != "switch_expression" {
        return None;
    }
    switch.child_by_field_name("condition")
}

/// The `new T(…)` whose class name — `Box` in `new a.Box<String>(…)` — is
/// at `offset`, with that name's span: where the constructor it selects is
/// asked for. `None` on a qualifier, a type argument, the keyword, and for
/// `outer.new Inner()`, whose type the name alone does not say.
pub fn creation_at(
    document: &TextDocument,
    tree: &SyntaxTree,
    offset: u32,
) -> Option<(Span, Expr)> {
    let name = crate::queries::identifier_at(tree.root(), offset)?;
    let creation = creation_named(name)?;
    match project(document, creation, 0) {
        found @ Expr::New { .. } => Some((span_of(document, name), found)),
        _ => None,
    }
}

/// The `this(…)` or `super(…)` whose keyword is at `offset` — and, with
/// `constants`, the enum constant whose name is — with that token's span,
/// what it constructs, and what it passes. What it constructs is
/// `Expr::This` for `this(…)` and for a constant, which constructs the enum
/// around it, and `Expr::Super` for `super(…)`.
///
/// `None` for `outer.super(…)`, whose superclass's enclosing instance the
/// call names, and for `<T>this(…)`, which writes what selection would infer.
pub fn explicit_constructor_call_at(
    document: &TextDocument,
    tree: &SyntaxTree,
    offset: u32,
    constants: bool,
) -> Option<(Span, Expr, Vec<Expr>)> {
    let at = |offset: usize| tree.root().descendant_for_byte_range(offset, offset);
    let offset = offset as usize;
    [Some(offset), offset.checked_sub(1)]
        .into_iter()
        .flatten()
        .filter_map(at)
        .find_map(|token| {
            let call = constructor_call_named(token, constants)?;
            let receiver = match token.kind() {
                "super" => Expr::Super {
                    span: span_of(document, token),
                },
                "this" => Expr::This {
                    span: span_of(document, token),
                },
                // Read where the enum's body opens: a constant with a body is
                // an anonymous class, inside which no enclosing type is known.
                _ => Expr::This {
                    span: span_of(
                        document,
                        call.parent()
                            .filter(|body| body.kind() == "enum_body")?
                            .child(0)?,
                    ),
                },
            };
            Some((
                span_of(document, token),
                receiver,
                project_arguments(document, call, 1),
            ))
        })
}

/// The explicit constructor invocation `token` is the `this` or `super` of,
/// or — with `constants` — the enum constant it is the name of.
fn constructor_call_named(token: Node<'_>, constants: bool) -> Option<Node<'_>> {
    let parent = token.parent()?;
    (constructor_call_token(parent, constants) == Some(token)).then_some(parent)
}

/// The token `call` is asked at, when it is an unqualified `this(…)` or
/// `super(…)`, its keyword, or — with `constants` — an enum constant, its
/// name. Read downwards, so a walk over every node asks no node its parent,
/// which tree-sitter finds by searching from the root.
pub(crate) fn constructor_call_token(call: Node<'_>, constants: bool) -> Option<Node<'_>> {
    match call.kind() {
        "explicit_constructor_invocation"
            if call.child_by_field_name("object").is_none()
                && call.child_by_field_name("type_arguments").is_none() =>
        {
            call.child_by_field_name("constructor")
                .filter(|keyword| matches!(keyword.kind(), "this" | "super"))
        }
        "enum_constant" if constants => call.child_by_field_name("name"),
        _ => None,
    }
}

/// The object creation `name` is the class name of.
pub(crate) fn creation_named(name: Node<'_>) -> Option<Node<'_>> {
    if name.kind() != "type_identifier" {
        return None;
    }
    let mut ty = name;
    let mut parent = name.parent()?;
    // `a.Box`'s last segment, and `Box<String>`'s type.
    if parent.kind() == "scoped_type_identifier" {
        let last = parent.named_child(
            u32::try_from(parent.named_child_count())
                .ok()?
                .checked_sub(1)?,
        )?;
        if last != name {
            return None;
        }
        ty = parent;
        parent = parent.parent()?;
    }
    if parent.kind() == "generic_type" {
        if parent.named_child(0)? != ty {
            return None;
        }
        ty = parent;
        parent = parent.parent()?;
    }
    (parent.kind() == "object_creation_expression"
        && parent.child_by_field_name("type") == Some(ty))
    .then_some(parent)
}

/// Climb from a token to the widest expression that still ends where it does.
///
/// `b.child()` parses with `b` several levels below the call; the receiver of
/// the next `.` is the call, not the name.
fn largest_expression_ending_at<'t>(node: Node<'t>, end: u32) -> Node<'t> {
    let mut best = node;
    let mut current = node;
    while let Some(parent) = current.parent() {
        if parent.end_byte() as u32 != end {
            break;
        }
        best = parent;
        current = parent;
    }
    best
}

/// Project a node onto the closed [`Expr`] list.
///
/// Every arm names a grammar node kind, which is the point: this is the only
/// file in the workspace outside `syntax.rs` and `scopes.rs` allowed to.
pub(crate) fn project(document: &TextDocument, node: Node<'_>, depth: u32) -> Expr {
    let span = span_of(document, node);
    if depth > MAX_CHAIN {
        return Expr::Opaque { span };
    }
    match node.kind() {
        "lambda_expression" => Expr::Lambda {
            results: node.child_by_field_name("body").and_then(|body| {
                if body.kind() != "block" {
                    return Some(vec![project(document, body, depth + 1)]);
                }
                let returns = block_returns(body)?;
                let values = returns
                    .into_iter()
                    .map(|ret| {
                        ret.named_children(&mut ret.walk())
                            .find(|value| !value.is_extra())
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some(
                    values
                        .into_iter()
                        .map(|value| project(document, value, depth + 1))
                        .collect(),
                )
            }),
            typed: node
                .child_by_field_name("parameters")
                .is_some_and(|parameters| parameters.kind() == "formal_parameters"),
            span,
        },
        // `Foo::<T>bar` writes what binding would infer, as a call that
        // writes type arguments does.
        "method_reference" if named_kind(node, "type_arguments") => Expr::Opaque { span },
        "method_reference" => match node.named_child(0) {
            Some(left) => {
                let name = node.named_child((node.named_child_count() as u32).saturating_sub(1));
                Expr::MethodRef {
                    target: Box::new(project(document, left, depth + 1)),
                    type_text: matches!(
                        left.kind(),
                        "generic_type"
                            | "array_type"
                            | "scoped_type_identifier"
                            | "type_identifier"
                            | "integral_type"
                            | "floating_point_type"
                            | "boolean_type"
                    )
                    .then(|| text(document, left)),
                    name: name
                        .filter(|name| name.kind() == "identifier" && name.id() != left.id())
                        .map(|name| text(document, name)),
                    span,
                }
            }
            None => Expr::Opaque { span },
        },
        // A type name lexes as `type_identifier` rather than `identifier` once
        // recovery loses the right-hand side of a method reference (`b::`) —
        // both are bare names, which is what `Expr::Name` means.
        "identifier" | "type_identifier" => Expr::Name {
            name: text(document, node),
            span,
        },
        "this" => Expr::This { span },
        // `Outer.super` and `Iface.super` name a supertype of another type
        // than the enclosing one.
        "super" if node.prev_sibling().is_some_and(|dot| dot.kind() == ".") => {
            Expr::Opaque { span }
        }
        "super" => Expr::Super { span },
        "parenthesized_expression" => match node.named_child(0) {
            Some(inner) => project(document, inner, depth + 1),
            None => Expr::Opaque { span },
        },
        "field_access" => {
            match (
                node.child_by_field_name("object"),
                node.child_by_field_name("field"),
            ) {
                (Some(object), Some(field)) => Expr::FieldAccess {
                    receiver: Box::new(project(document, object, depth + 1)),
                    name: text(document, field),
                    name_span: span_of(document, field),
                },
                _ => Expr::Opaque { span },
            }
        }
        "method_invocation" => match node.child_by_field_name("name") {
            Some(name) => Expr::Call {
                receiver: node
                    .child_by_field_name("object")
                    .map(|object| Box::new(project(document, object, depth + 1))),
                name: text(document, name),
                name_span: span_of(document, name),
                arguments: project_arguments(document, node, depth + 1),
                type_arguments: node
                    .child_by_field_name("type_arguments")
                    .map(|written| {
                        written
                            .named_children(&mut written.walk())
                            .map(|argument| text(document, argument))
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            None => Expr::Opaque { span },
        },
        // `outer.new Inner()` creates a member type of `outer`'s, which the
        // name alone does not say.
        "object_creation_expression"
            if node.child(0).is_some_and(|first| first.kind() != "new") =>
        {
            Expr::Opaque { span }
        }
        "object_creation_expression" => match node.child_by_field_name("type") {
            Some(ty) => Expr::New {
                type_text: text(document, ty),
                anonymous: node
                    .named_children(&mut node.walk())
                    .any(|child| child.kind() == "class_body"),
                arguments: project_arguments(document, node, depth + 1),
                span,
            },
            None => Expr::Opaque { span },
        },
        "array_access" => match node.child_by_field_name("array") {
            Some(array) => Expr::ArrayAccess {
                array: Box::new(project(document, array, depth + 1)),
                span,
            },
            None => Expr::Opaque { span },
        },
        "cast_expression" => {
            match (
                node.child_by_field_name("type"),
                node.child_by_field_name("value"),
            ) {
                // `(A & B) o` has the members of both, and `type_text` can
                // name one.
                (Some(_), Some(_))
                    if node
                        .children_by_field_name("type", &mut node.walk())
                        .nth(1)
                        .is_some() =>
                {
                    Expr::Opaque { span }
                }
                (Some(ty), Some(value)) => Expr::Cast {
                    type_text: text(document, ty),
                    inner: Box::new(project(document, value, depth + 1)),
                    span,
                },
                _ => Expr::Opaque { span },
            }
        }
        // An integer literal's kind does not say `int` from `long`, nor a
        // floating-point literal's `float` from `double` — that is the
        // `l`/`L` or `f`/`F` suffix on the source text, which the grammar
        // treats as part of the token rather than a field.
        "decimal_integer_literal"
        | "hex_integer_literal"
        | "octal_integer_literal"
        | "binary_integer_literal" => Expr::Literal {
            literal: if ends_with(document, node, ['l', 'L']) {
                Literal::Long
            } else {
                Literal::Int
            },
            span,
        },
        "decimal_floating_point_literal" | "hex_floating_point_literal" => Expr::Literal {
            literal: if ends_with(document, node, ['f', 'F']) {
                Literal::Float
            } else {
                Literal::Double
            },
            span,
        },
        "character_literal" => Expr::Literal {
            literal: Literal::Char,
            span,
        },
        "true" | "false" => Expr::Literal {
            literal: Literal::Boolean,
            span,
        },
        // Also a text block: both lex as `string_literal` in this grammar.
        "string_literal" => Expr::Literal {
            literal: Literal::String,
            span,
        },
        "null_literal" => Expr::Literal {
            literal: Literal::Null,
            span,
        },
        "class_literal" => match node.named_child(0) {
            Some(ty) => Expr::ClassLiteral {
                type_text: text(document, ty),
                span,
            },
            None => Expr::Opaque { span },
        },
        _ => Expr::Opaque { span },
    }
}

/// Every argument of a call's argument list, projected in order.
///
/// Shared by `project`'s `method_invocation` arm and `member_selection_at`'s
/// `SelectionKind::Call`, so a nested call inside an argument list is bounded
/// by the same `MAX_CHAIN` cap either way.
fn project_arguments(document: &TextDocument, call: Node<'_>, depth: u32) -> Vec<Expr> {
    match call.child_by_field_name("arguments") {
        Some(args) => {
            let mut cursor = args.walk();
            args.named_children(&mut cursor)
                // `line_comment` and `block_comment` are `extras` in the grammar,
                // which makes them named children of `argument_list` too — so an
                // uncommented `f(1)` and a commented `f(1 /*c*/)` would otherwise
                // project a different number of arguments for the same call.
                .filter(|arg| !arg.is_extra())
                .map(|arg| project(document, arg, depth))
                .collect()
        }
        None => Vec::new(),
    }
}

/// More `return`s than this in one lambda block read as no result at all.
const MAX_RETURNS: usize = 32;

/// The `return` statements a lambda's block owns — not a nested lambda's, nor
/// a local or anonymous class's — or `None` past [`MAX_RETURNS`] or with none.
/// Iterative, and each node of the block is visited once.
fn block_returns(block: Node<'_>) -> Option<Vec<Node<'_>>> {
    let mut returns = Vec::new();
    let mut stack = vec![block];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "return_statement" => {
                if returns.len() == MAX_RETURNS {
                    return None;
                }
                returns.push(node);
                continue;
            }
            "lambda_expression"
            | "class_body"
            | "class_declaration"
            | "record_declaration"
            | "enum_declaration"
            | "interface_declaration" => continue,
            _ => {}
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    (!returns.is_empty()).then_some(returns)
}

/// Whether `node` has a named child of `kind`.
fn named_kind(node: Node<'_>, kind: &str) -> bool {
    node.named_children(&mut node.walk())
        .any(|child| child.kind() == kind)
}

/// Whether a literal token ends with one of the given suffix characters.
fn ends_with<const N: usize>(document: &TextDocument, node: Node<'_>, suffixes: [char; N]) -> bool {
    text(document, node)
        .chars()
        .next_back()
        .is_some_and(|c| suffixes.contains(&c))
}

/// Source text of a node. Slicing rather than `utf8_text` so a node whose
/// range the grammar reported oddly yields an empty string instead of an error.
fn text(document: &TextDocument, node: Node<'_>) -> String {
    document
        .text()
        .get(node.start_byte()..node.end_byte())
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JavaParser;
    use javelin_document::{DocumentId, TextDocument};

    fn selection(source: &str) -> Option<Selection> {
        let offset = source.find('|').expect("mark the cursor with |") as u32;
        let source = source.replace('|', "");
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 1, source);
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        member_selection_at(&document, &tree, offset)
    }

    /// The projected arguments of `f(<expr_source>)`'s call, via the same
    /// `unqualified_call_at` an unqualified call is resolved through.
    fn call_arguments(expr_source: &str) -> Vec<Expr> {
        let source = format!("class A {{ void m() {{ f({expr_source}); }} }}");
        let offset = source.find('f').expect("the call") as u32;
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 1, source);
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        let Some(Expr::Call { arguments, .. }) = unqualified_call_at(&document, &tree, offset)
        else {
            panic!("expected a call");
        };
        arguments
    }

    /// The `Literal` a single argument expression projects as.
    fn literal_of(expr_source: &str) -> Literal {
        let arguments = call_arguments(expr_source);
        let [Expr::Literal { literal, .. }] = arguments.as_slice() else {
            panic!("expected one literal argument, got {arguments:?}");
        };
        *literal
    }

    #[test]
    fn a_case_constant_carries_its_switchs_selector() {
        let case = |source: &str, at: &str| {
            let offset = source.find(at).expect("the cursor") as u32;
            let document = TextDocument::new(DocumentId::new("file:///A.java"), 1, source);
            let mut parser = JavaParser::new().expect("grammar loads");
            let tree = parser.parse(&document).expect("parses");
            case_constant_at(&document, &tree, offset).map(|(name, _, selector)| (name, selector))
        };
        let source = "class A { int f(Day d) { switch (d) { case MON: break; case TUE, WED: \
                      break; } return switch (this.d) { case MON -> 1; case Day.TUE -> 2; \
                      case Shape s when s.x() > 0 -> 3; default -> MON; }; } }";
        let (name, selector) = case(source, "MON:").expect("a colon-form constant");
        assert_eq!(name, "MON");
        assert!(matches!(selector, Expr::Name { name, .. } if name == "d"));
        assert_eq!(case(source, "WED").map(|it| it.0).as_deref(), Some("WED"));
        let (_, selector) = case(source, "MON ->").expect("an arrow-form constant");
        assert!(matches!(selector, Expr::FieldAccess { name, .. } if name == "d"));
        assert_eq!(case(source, "TUE ->"), None, "a qualified constant");
        assert_eq!(case(source, "s when"), None, "a pattern's variable");
        assert_eq!(case(source, "x()"), None, "a guard");
        assert_eq!(case(source, "MON; }"), None, "a default's body");
    }

    #[test]
    fn a_method_reference_carries_its_target_type_text_and_name() {
        let reference = |source: &str| match call_arguments(source).as_slice() {
            [
                Expr::MethodRef {
                    target,
                    type_text,
                    name,
                    ..
                },
            ] => (target.as_ref().clone(), type_text.clone(), name.clone()),
            other => panic!("expected one method reference, got {other:?}"),
        };
        let (target, type_text, name) = reference("Order::getId");
        assert!(matches!(target, Expr::Name { name, .. } if name == "Order"));
        assert_eq!((type_text, name.as_deref()), (None, Some("getId")));
        let (target, _, name) = reference("Map.Entry::getKey");
        assert!(matches!(target, Expr::FieldAccess { name, .. } if name == "Entry"));
        assert_eq!(name.as_deref(), Some("getKey"));
        let (_, type_text, name) = reference("List<String>::size");
        assert_eq!(
            (type_text.as_deref(), name.as_deref()),
            (Some("List<String>"), Some("size"))
        );
        let (_, type_text, _) = reference("int[]::clone");
        assert_eq!(type_text.as_deref(), Some("int[]"));
        let (target, _, name) = reference("Box::new");
        assert!(matches!(target, Expr::Name { name, .. } if name == "Box"));
        assert_eq!(name, None, "`::new` names no method");
        assert!(matches!(reference("this::f").0, Expr::This { .. }));
        assert!(matches!(reference("x.y()::z").0, Expr::Call { .. }));
        assert!(
            matches!(
                call_arguments("Foo::<T>bar").as_slice(),
                [Expr::Opaque { .. }]
            ),
            "explicit type arguments are opaque, as on a call"
        );
    }

    #[test]
    fn a_lambdas_results_are_its_body_or_its_blocks_own_returns() {
        let results = |source: &str| match call_arguments(source).as_slice() {
            [Expr::Lambda { results, .. }] => results.clone().map(|results| {
                results
                    .iter()
                    .map(|result| match result {
                        Expr::Name { name, .. } => name.clone(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
            }),
            other => panic!("expected one lambda, got {other:?}"),
        };
        assert_eq!(results("x -> y"), Some(vec!["y".into()]));
        assert_eq!(
            results("x -> { if (x) return a; return /* c */ b; }"),
            Some(vec!["a".into(), "b".into()])
        );
        assert_eq!(
            results(
                "x -> { g(y -> { return c; }); new Object() { Object f() { return d; } }; \
                 class L { Object f() { return e; } } return a; }"
            ),
            Some(vec!["a".into()]),
            "a nested lambda's and a local or anonymous class's returns are theirs"
        );
        assert_eq!(results("x -> { return; }"), None, "not value-compatible");
        assert_eq!(results("x -> { f(x); }"), None, "no return");
        let many = "if (x) return a; ".repeat(MAX_RETURNS + 1);
        assert_eq!(
            results(&format!("x -> {{ {many} }}")),
            None,
            "too many to read"
        );
    }

    #[test]
    fn a_class_literal_carries_its_type_as_written() {
        for (source, written) in [("Repo.class", "Repo"), ("int[].class", "int[]")] {
            let arguments = call_arguments(source);
            assert!(
                matches!(
                    arguments.as_slice(),
                    [Expr::ClassLiteral { type_text, .. }] if type_text == written
                ),
                "{source}: {arguments:?}"
            );
        }
    }

    #[test]
    fn a_dot_after_a_local_selects_from_that_name() {
        let found = selection("class A { void m() { Box b; b.| } }").expect("a selection");
        assert!(matches!(&found.receiver, Expr::Name { name, .. } if name == "b"));
        assert_eq!(found.prefix, "");
    }

    #[test]
    fn a_partial_member_name_is_the_prefix() {
        let found = selection("class A { void m() { Box b; b.wid| } }").expect("a selection");
        assert_eq!(found.prefix, "wid");
        assert_eq!(found.name, "wid");
        assert!(found.name_span.is_some());
    }

    #[test]
    fn a_cursor_inside_a_member_name_sees_the_prefix_and_the_whole_name() {
        // Completion filters on "wid"; definition must look up "width".
        let found = selection("class A { void m() { Box b; b.wid|th } }").expect("a selection");
        assert_eq!(found.prefix, "wid");
        assert_eq!(found.name, "width");
    }

    #[test]
    fn a_varargs_parameter_being_typed_is_not_a_selection() {
        assert!(selection("class A { void m(int...| a) {} }").is_none());
    }

    #[test]
    fn a_chained_call_projects_its_whole_receiver() {
        let found = selection("class A { void m() { b.child().| } }").expect("a selection");
        let Expr::Call { receiver, name, .. } = &found.receiver else {
            panic!("expected a call, got {:?}", found.receiver);
        };
        assert_eq!(name, "child");
        assert!(matches!(receiver.as_deref(), Some(Expr::Name { .. })));
    }

    #[test]
    fn this_and_super_project_as_themselves() {
        assert!(matches!(
            selection("class A { void m() { this.| } }")
                .expect("a selection")
                .receiver,
            Expr::This { .. }
        ));
        assert!(matches!(
            selection("class A { void m() { super.| } }")
                .expect("a selection")
                .receiver,
            Expr::Super { .. }
        ));
    }

    #[test]
    fn a_new_expression_projects_the_type_it_creates() {
        let found = selection("class A { void m() { new Box().| } }").expect("a selection");
        assert!(matches!(
            &found.receiver,
            Expr::New { type_text, anonymous: false, .. } if type_text == "Box"
        ));
    }

    /// An anonymous class's own methods are callable on the creation, so a
    /// caller that claims a member is absent must know it is one.
    #[test]
    fn an_anonymous_creation_says_so() {
        let found = selection("class A { void m() { new Runnable() { public void run() {} }.| } }")
            .expect("a selection");
        assert!(matches!(
            &found.receiver,
            Expr::New { type_text, anonymous: true, .. } if type_text == "Runnable"
        ));
    }

    /// `this(…)` and `super(…)` are answered at their keyword, an enum
    /// constant at its name only when asked; a qualified `super` and a
    /// call writing type arguments are not.
    #[test]
    fn an_explicit_constructor_call_is_answered_at_its_keyword_with_its_arguments() {
        let source = "enum E { A(1, 2), B { }, C; E(int a, int b) { } E() { } }\n\
                      class K extends B { K() { this(1); } K(int a) { super(a, 2); }\n\
                      K(O o) { o.super(); } K(char c) { <String>this(); } }";
        let document = TextDocument::new(DocumentId::new("file:///E.java"), 1, source.to_owned());
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        let at = |needle: &str, constants: bool| {
            explicit_constructor_call_at(
                &document,
                &tree,
                source.find(needle).unwrap() as u32,
                constants,
            )
        };
        let text = |span: Span| &source[span.start_byte as usize..span.end_byte as usize];
        let (span, receiver, arguments) = at("this(1)", false).expect("this(…)");
        assert_eq!(text(span), "this");
        assert!(matches!(receiver, Expr::This { .. }));
        assert_eq!(arguments.len(), 1);
        let (_, receiver, arguments) = at("super(a", false).expect("super(…)");
        assert!(matches!(receiver, Expr::Super { .. }));
        assert_eq!(arguments.len(), 2);
        let (span, receiver, arguments) = at("A(1", true).expect("a constant");
        assert_eq!(text(span), "A");
        assert!(matches!(receiver, Expr::This { .. }));
        assert_eq!(arguments.len(), 2);
        assert_eq!(at("B {", true).expect("no arguments").2.len(), 0);
        assert!(at("A(1", false).is_none());
        assert!(at("super()", false).is_none());
        assert!(at("this()", false).is_none());
        assert!(at("1, 2)", true).is_none());
        let listed: Vec<&str> = crate::explicit_constructor_spans(
            &document,
            &tree,
            &javelin_core::CancellationToken::new(),
        )
        .expect("not cancelled")
        .into_iter()
        .map(text)
        .collect();
        assert_eq!(listed, ["A", "B", "C", "this", "super"]);
    }

    /// The class name is where a constructor is asked for; a qualifier, a
    /// type argument and the keyword are not it.
    #[test]
    fn a_creation_is_answered_at_its_class_name_with_its_arguments() {
        let source = "class A { void m() { new a.Box<String>(1, b) { }; outer.new Inner(); } }";
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 1, source.to_owned());
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        let at = |needle: &str| creation_at(&document, &tree, source.find(needle).unwrap() as u32);
        let (name, found) = at("Box").expect("the class name");
        assert_eq!(
            &source[name.start_byte as usize..name.end_byte as usize],
            "Box"
        );
        let Expr::New {
            type_text,
            arguments,
            anonymous: true,
            ..
        } = found
        else {
            panic!("a creation, not {found:?}");
        };
        assert_eq!(type_text, "a.Box<String>");
        assert_eq!(arguments.len(), 2);
        assert!(at("a.Box").is_none());
        assert!(at("String").is_none());
        assert!(at("new a").is_none());
        assert!(at("Inner").is_none());
    }

    #[test]
    fn a_cast_and_an_array_access_project_their_parts() {
        let found = selection("class A { void m() { ((Box) o).| } }").expect("a selection");
        assert!(matches!(&found.receiver, Expr::Cast { type_text, .. } if type_text == "Box"));
        let found = selection("class A { void m() { boxes[0].| } }").expect("a selection");
        assert!(matches!(found.receiver, Expr::ArrayAccess { .. }));
    }

    /// Each names a type its text alone does not: another type's supertype,
    /// two types at once, a member type of the qualifier's.
    #[test]
    fn a_qualified_super_an_intersection_cast_and_a_qualified_creation_are_opaque() {
        for source in [
            "class A { void m() { Outer.super.| } }",
            "class A { void m() { ((Box & Runnable) o).| } }",
            "class A { void m() { outer.new Inner().| } }",
        ] {
            let found = selection(source).expect(source);
            assert!(matches!(found.receiver, Expr::Opaque { .. }), "{source}");
        }
    }

    #[test]
    fn an_unlisted_expression_form_projects_as_opaque() {
        let found = selection("class A { void m() { (1 + 2).| } }").expect("a selection");
        assert!(matches!(found.receiver, Expr::Opaque { .. }));
    }

    #[test]
    fn a_cursor_that_is_not_selecting_a_member_has_no_selection() {
        assert!(selection("class A { void m() { int x = 1; x| } }").is_none());
    }

    #[test]
    fn a_method_reference_is_a_selection_too() {
        let found = selection("class A { void m() { b::| } }").expect("a selection");
        assert!(matches!(&found.receiver, Expr::Name { name, .. } if name == "b"));
    }

    #[test]
    fn a_varargs_ellipsis_is_not_a_selection() {
        assert!(selection("class A { void m(int... a) { a| } }").is_none());
    }

    #[test]
    fn whitespace_before_the_dot_still_selects_from_the_receiver() {
        let found = selection("class A { void m() { Box b; b . | } }").expect("a selection");
        assert!(matches!(&found.receiver, Expr::Name { name, .. } if name == "b"));
        let found = selection("class A { void m() { Box b; b . w| } }").expect("a selection");
        assert!(matches!(&found.receiver, Expr::Name { name, .. } if name == "b"));
        assert_eq!(found.prefix, "w");
    }

    #[test]
    fn a_leading_dot_chain_across_lines_projects_its_whole_receiver() {
        // What IntelliJ formats a long chain as, so it is ordinary input.
        let found =
            selection("class A { void m() { b\n  .child()\n  .w| ; } }").expect("a selection");
        let Expr::Call { receiver, name, .. } = &found.receiver else {
            panic!(
                "expected the call to be the receiver, got {:?}",
                found.receiver
            );
        };
        assert_eq!(name, "child");
        assert!(matches!(receiver.as_deref(), Some(Expr::Name { .. })));
        assert_eq!(found.prefix, "w");
    }

    #[test]
    fn an_unqualified_call_is_found_from_anywhere_in_its_name() {
        let source = "class A { int count() { return 1; } void m() { count(); } }";
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 1, source);
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        // The `count` of `count();`, not the one in its declaration.
        let start = source.rfind("count").expect("the call") as u32;

        // Start, middle, and immediately after the name: a cursor that has just
        // finished typing the call is as much on it as one inside it.
        for offset in [start, start + 2, start + 5] {
            let found = unqualified_call_at(&document, &tree, offset)
                .unwrap_or_else(|| panic!("a call at {offset}"));
            let Expr::Call {
                receiver,
                name,
                name_span,
                arguments,
                ..
            } = found
            else {
                panic!("expected a call at {offset}");
            };
            assert!(receiver.is_none(), "at {offset}");
            assert_eq!(name, "count", "at {offset}");
            assert_eq!(name_span.start_byte, start, "at {offset}");
            assert_eq!(arguments.len(), 0, "at {offset}");
        }
    }

    #[test]
    fn a_qualified_call_is_not_an_unqualified_one() {
        let source = "class A { void m() { o.count(); } }";
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 1, source);
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        let at = source.find("count").expect("the call") as u32 + 2;
        assert!(unqualified_call_at(&document, &tree, at).is_none());
    }

    #[test]
    fn an_offset_past_the_end_returns_nothing_instead_of_panicking() {
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 1, "class A {}");
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        assert!(member_selection_at(&document, &tree, 9_999).is_none());
    }

    #[test]
    fn an_offset_off_a_character_boundary_returns_nothing_instead_of_panicking() {
        let source = "class A { void m() { b.é } }";
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 1, source);
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        let middle = source.find('é').expect("the two-byte character") as u32 + 1;
        assert!(!source.is_char_boundary(middle as usize));
        assert!(member_selection_at(&document, &tree, middle).is_none());
    }

    #[test]
    fn each_literal_form_projects_the_matching_literal() {
        let cases: &[(&str, Literal)] = &[
            ("1", Literal::Int),
            ("1L", Literal::Long),
            ("0x1f", Literal::Int),
            ("010", Literal::Int),
            ("0b1", Literal::Int),
            ("1.5", Literal::Double),
            ("1.5f", Literal::Float),
            ("1e3", Literal::Double),
            ("1f", Literal::Float),
            ("1d", Literal::Double),
            ("0x1.8p1", Literal::Double),
            ("0x1.8p1f", Literal::Float),
            ("'c'", Literal::Char),
            ("true", Literal::Boolean),
            ("false", Literal::Boolean),
            ("\"s\"", Literal::String),
            (
                "\"\"\"\n            hi\n            \"\"\"",
                Literal::String,
            ),
            ("null", Literal::Null),
        ];
        for (source, expected) in cases {
            assert_eq!(literal_of(source), *expected, "for {source:?}");
        }
    }

    #[test]
    fn a_calls_modelled_arguments_project_as_themselves_in_order() {
        let arguments = call_arguments("1, new Box(), b");
        assert!(matches!(
            &arguments[0],
            Expr::Literal {
                literal: Literal::Int,
                ..
            }
        ));
        assert!(matches!(&arguments[1], Expr::New { type_text, .. } if type_text == "Box"));
        assert!(matches!(&arguments[2], Expr::Name { name, .. } if name == "b"));
    }

    #[test]
    fn an_unmodelled_argument_form_still_occupies_its_position_as_opaque() {
        // The old `arity` was `args.named_child_count()`; the projected list's
        // length must still match it, so a form `Expr` does not model cannot
        // be dropped from the list. Comments are the one named child of
        // `argument_list` deliberately *not* counted — see the test below.
        let arguments = call_arguments("1, 1 + 2, b");
        assert_eq!(arguments.len(), 3);
        assert!(matches!(&arguments[1], Expr::Opaque { .. }));
    }

    #[test]
    fn a_comment_in_the_argument_list_does_not_inflate_the_arity() {
        // `line_comment` and `block_comment` are named `extras`, so they show up
        // as named children of `argument_list` alongside the real arguments.
        // Counting them would make `f(1 /*c*/)` a two-argument call.
        assert_eq!(call_arguments("1 /*c*/").len(), 1);
        assert_eq!(call_arguments("/* why */ 1").len(), 1);
        assert_eq!(call_arguments("1, // first\n        2").len(), 2);
    }

    #[test]
    fn a_nested_call_argument_projects_as_a_call() {
        let arguments = call_arguments("h(1)");
        let [
            Expr::Call {
                name,
                arguments: inner,
                ..
            },
        ] = arguments.as_slice()
        else {
            panic!("expected one nested call argument, got {arguments:?}");
        };
        assert_eq!(name, "h");
        assert!(matches!(
            inner.as_slice(),
            [Expr::Literal {
                literal: Literal::Int,
                ..
            }]
        ));
    }

    #[test]
    fn a_call_selects_with_its_arguments() {
        let found = selection("class A { void m() { Box b; b.g|(1); } }").expect("a selection");
        let SelectionKind::Call { arguments } = &found.kind else {
            panic!("expected a call selection, got {:?}", found.kind);
        };
        assert!(matches!(
            arguments.as_slice(),
            [Expr::Literal {
                literal: Literal::Int,
                ..
            }]
        ));
    }

    #[test]
    fn a_value_selects_with_no_arguments() {
        let found = selection("class A { void m() { Box b; b.width|; } }").expect("a selection");
        assert_eq!(found.kind, SelectionKind::Value);
    }

    #[test]
    fn a_method_reference_selects_as_a_method_ref() {
        let found = selection("class A { void m() { b::label| ; } }").expect("a selection");
        assert_eq!(found.kind, SelectionKind::MethodRef);
    }

    #[test]
    fn a_dot_with_nothing_after_it_selects_a_value() {
        let found = selection("class A { void m() { Box b; b.| } }").expect("a selection");
        assert_eq!(found.kind, SelectionKind::Value);
    }
}
