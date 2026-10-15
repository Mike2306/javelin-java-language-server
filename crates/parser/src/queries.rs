//! What a position in one file refers to, and what could be written there.
//!
//! This is the engine side of go-to-definition, hover and completion. It answers
//! from the open buffer and nothing else: the scope tree for the names the file
//! binds, the syntax view for doc comments and for the declarations the scope
//! tree deliberately does not bind — methods and constructors, which cannot be
//! resolved from a call site without a type model.
//!
//! Everything it reports is text the author wrote. A type here is the spelling
//! that stands where a type goes, never a resolved one, and a name this file
//! does not declare has no answer rather than a plausible one. Guessing at
//! `List` because some local is also called `List` is worse than saying nothing:
//! the editor jumps somewhere wrong and the user stops trusting it.

use std::collections::HashMap;

use javelin_core::{CancellationToken, Cancelled};
use javelin_document::{DocumentId, TextDocument};
use tree_sitter::Node;

use crate::SyntaxTree;
use crate::scopes::{Binding, BindingKind, ScopeTree};
use crate::syntax::{Decl, DeclKind, FileSyntax, Span};

/// What an identifier turned out to name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefinitionKind {
    LocalVariable,
    Parameter,
    /// Introduced by an `instanceof`, a `case` label or a record pattern.
    PatternBinding,
    Field,
    TypeParameter,
    /// A class, interface, enum, record or annotation type.
    Type,
    Method,
    Constructor,
}

/// Where a name was declared, and how the declaration reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    /// The file `span` and `name_span` are offsets into.
    ///
    /// Not necessarily the file the question was asked in — `reference` is
    /// the only span here that is. A caller that renders a location reads
    /// this rather than the URI the request named; the two were the same
    /// thing until a query could reach more than one file, and a caller that
    /// assumes they still are jumps into the wrong buffer.
    pub file: DocumentId,
    /// The identifier the question was asked about, so hover can highlight it.
    pub reference: Span,
    pub name: String,
    pub kind: DefinitionKind,
    /// The whole declaration.
    pub span: Span,
    /// Just the name: where go-to-definition lands.
    pub name_span: Span,
    /// The declaration as written, with any body left off.
    pub signature: String,
    /// The `/** … */` above the declaration, verbatim and unparsed.
    pub doc: Option<String>,
}

/// Resolve the identifier at a byte offset against what this file declares.
///
/// `None` covers every honest miss: the offset is on whitespace, on a keyword,
/// on a member selected from an expression, or on a name that comes from an
/// import, a supertype or the JDK. None of those are errors — they are the
/// normal state of a file being read at Phase 1 readiness.
pub fn definition_at(
    document: &TextDocument,
    tree: &SyntaxTree,
    scopes: &ScopeTree,
    syntax: &FileSyntax,
    offset: u32,
) -> Option<Definition> {
    let identifier = identifier_at(tree.root(), offset).filter(|node| is_simple_name(*node))?;
    let reference = span(document, identifier);
    let name = text(document, identifier)?;

    // The cursor is on a declaration's own name. That declaration is the answer
    // whatever the scope tree says, and it is the only way to reach a method or
    // a constructor, which are bound nowhere.
    if let Some(decl) = syntax.declaration_at(reference) {
        return Some(Definition {
            file: document.id().clone(),
            reference,
            name: decl.name.clone(),
            kind: DefinitionKind::from(decl.kind),
            span: decl.span,
            name_span: decl.name_span,
            signature: signature_of(document, tree, decl.span),
            doc: decl.doc.clone(),
        });
    }

    // The grammar already separates Java's two namespaces where the syntax
    // settles it: a name written where only a type can go is a
    // `type_identifier`, and reading it as a variable would let a local hide a
    // class of the same name, which Java does not allow.
    //
    // A plain `identifier` is the ambiguous case — `Foo.bar()` and `Foo::bar`
    // read as a variable if one is in scope and as a type otherwise, which is
    // the order JLS 6.5.2 gives.
    let at = reference.start_byte;
    let binding = match identifier.kind() {
        "type_identifier" => scopes.resolve_type(at, name),
        _ => scopes
            .resolve_value(at, name)
            .or_else(|| scopes.resolve_type(at, name)),
    }?;
    Some(from_binding(document, tree, syntax, binding, reference))
}

/// The plain name written at a byte offset, and where it is.
///
/// [`definition_at`]'s first two steps without its third: the identifier at
/// `offset`, refused wherever that query refuses one — a member selected from
/// an expression, the name of a call, a label — and then *not* resolved
/// against anything.
///
/// It exists for the caller that has somewhere else to resolve it.
/// `javelin-semantic` resolves it as a type, against the files a query can
/// reach and then the classpath, which is how a name this file does not
/// declare comes to mean the file next door. This says nothing about whether
/// a type is what was written there, and nothing about whether the file binds
/// the name — a caller that cares asks the scope tree, in the same order
/// [`definition_at`] asks it.
pub fn name_at(document: &TextDocument, tree: &SyntaxTree, offset: u32) -> Option<(String, Span)> {
    let identifier = identifier_at(tree.root(), offset).filter(|node| is_simple_name(*node))?;
    Some((
        text(document, identifier)?.to_owned(),
        span(document, identifier),
    ))
}

/// A segment of a qualified type name at `offset` — `util` or `List` in
/// `java.util.List`, `Entry` in `Map.Entry`, a segment of an import or of an
/// annotation's name — with the dotted text up to and including it, which is
/// what that segment names, and its span. [`name_at`] refuses every one of
/// them; this answers them, and resolves nothing either.
///
/// Only where the grammar says a type or a package is meant: a
/// `scoped_type_identifier`, an import, an annotation's name. A dotted name in
/// an expression — `java.util.List.of()` — is a field access that may be a
/// variable's, and `Outer<T>.Inner` or an annotated segment has more than a
/// name in it; all of those answer `None`.
///
/// ponytail: `java.util.List.of()` therefore jumps nowhere from `List`. The
/// upgrade is JLS 6.5.2's reclassification — a field access whose left side
/// names no variable in scope is a package or a type — which needs the scope
/// tree asked about every prefix.
pub fn qualified_name_at(
    document: &TextDocument,
    tree: &SyntaxTree,
    offset: u32,
) -> Option<(String, Span)> {
    let identifier = identifier_at(tree.root(), offset)?;
    let segment = qualified_segment(document, identifier)?;
    Some((segment.dotted, span(document, identifier)))
}

/// One segment of a qualified name, as [`qualified_name_at`] reads it.
pub(crate) struct QualifiedSegment {
    /// The text from the name's start through this segment, dots only.
    pub(crate) dotted: String,
    /// Whether this is the name's last segment.
    pub(crate) last: bool,
    /// Whether the name is in a type position rather than an import.
    pub(crate) in_type: bool,
}

pub(crate) fn qualified_segment(
    document: &TextDocument,
    node: Node<'_>,
) -> Option<QualifiedSegment> {
    let scoped = node.parent()?;
    let kind = scoped.kind();
    if !matches!(kind, "scoped_type_identifier" | "scoped_identifier") {
        return None;
    }
    let mut outermost = scoped;
    while let Some(parent) = outermost.parent().filter(|parent| parent.kind() == kind) {
        outermost = parent;
    }
    let in_type = match kind {
        "scoped_type_identifier" => true,
        _ => {
            let parent = outermost.parent()?;
            match parent.kind() {
                "import_declaration" => false,
                "marker_annotation" | "annotation"
                    if parent.child_by_field_name("name") == Some(outermost) =>
                {
                    true
                }
                _ => return None,
            }
        }
    };
    let written = document
        .text()
        .get(outermost.start_byte()..node.end_byte())?;
    let dotted: String = written.chars().filter(|c| !c.is_whitespace()).collect();
    if !dotted
        .chars()
        .all(|c| c == '.' || c == '_' || c == '$' || c.is_alphanumeric())
    {
        return None;
    }
    Some(QualifiedSegment {
        dotted,
        last: node.end_byte() == outermost.end_byte(),
        in_type,
    })
}

/// Every occurrence, in this file, of the binding the name at `offset` refers
/// to — its declaration and every use that resolves to the same binding.
///
/// An empty result covers the same misses [`name_at`] does — whitespace, a
/// keyword, a member selected from an expression, the name of a call — plus
/// one more: a name that offset resolves to nothing the scope tree binds, per
/// Ruling 6.
///
/// The correctness bar is shadowing: an inner `int x` and an outer `int x` are
/// two different [`Binding`]s, and one is never mistaken for the other. That
/// is why this does not match by name — every identifier token in the file
/// spelled the same is a *candidate*, and a candidate only survives by
/// resolving, at its own offset, to the identical binding the cursor did.
/// Two declarations of one name at one span in two different files would
/// collide under a weaker key, which is exactly what `DeclSite` (3b's first
/// slice) exists to rule out one layer up; here, within one file, `Binding`'s
/// own span does the same job.
///
/// Unlike [`name_at`] and [`definition_at`], which each resolve one offset in
/// time bounded by the tree's depth, this walks every identifier token in the
/// file — work proportional to file size, the shape [`crate::folding_ranges`]
/// and [`crate::selection_ranges`] already take a token for. `token` is
/// checked once up front and again for every node the scan visits, so a
/// cancelled request on a large file stops at the next node rather than
/// finishing a walk nothing will read the answer to.
///
/// Returned spans are sorted by start offset and contain no duplicates, and
/// nothing here sorts or deduplicates them to make that true: the candidates
/// come from a private helper's pre-order walk over a tree whose nodes
/// already lie in source order, so it visits each one exactly once, in
/// strictly increasing `start_byte` order, and filtering that list preserves
/// the order and the uniqueness it did not introduce. A change that merges a
/// second, differently-ordered source into this list — the member walk Phase
/// 3b's roadmap plans is exactly that — has to re-establish both properties
/// explicitly; nothing here still does it for free at that point.
///
/// ponytail: a method's or a constructor's name lives in [`FileSyntax`] and is
/// never bound by [`ScopeTree`] — the same gap [`definition_at`]'s
/// `is_simple_name` guard is built around — so clicking either highlights
/// nothing. Both halves of the upgrade exist above this crate:
/// `javelin_semantic::member_definition_at` and `call_definition_at` say what a
/// member name means, and the server's member references already keep only
/// the sites resolving to one declaration. The upgrade is the highlight
/// handler asking that, scoped to the open file, rather than this query.
pub fn occurrences_at(
    document: &TextDocument,
    tree: &SyntaxTree,
    scopes: &ScopeTree,
    offset: u32,
    token: &CancellationToken,
) -> Result<Vec<Span>, Cancelled> {
    token.check()?;
    let Some((name, reference)) = name_at(document, tree, offset) else {
        return Ok(Vec::new());
    };
    // `name_at` already ran the offset refusal and the `is_simple_name`
    // filter; fetched again only for the node's own kind, which namespace()
    // needs and `name_at` deliberately does not expose — see its own doc.
    let Some(node) = identifier_at(tree.root(), reference.start_byte) else {
        return Ok(Vec::new());
    };
    let Some(cursor) = namespace(scopes, node, &name) else {
        return Ok(Vec::new());
    };

    let mut candidates = Vec::new();
    collect_identifier_tokens(tree.root(), document, &name, &mut candidates, token)?;

    let spans: Vec<Span> = candidates
        .into_iter()
        .filter(|candidate| namespace(scopes, *candidate, &name) == Some(cursor))
        .map(|candidate| span(document, candidate))
        .collect();
    Ok(spans)
}

/// The binding `name` refers to at a token's own offset, in the namespace the
/// token's own grammar kind says it belongs to.
///
/// The same order [`definition_at`] asks the scope tree in: `resolve_type` for
/// a `type_identifier`, `resolve_value` with a `resolve_type` fallback for the
/// ambiguous plain `identifier` (JLS 6.5.2 — a value in scope wins, and only a
/// type answers where none is). Reusing that order rather than inventing
/// another is what keeps an occurrence and a jump to its definition from ever
/// disagreeing about which binding a position names.
pub(crate) fn namespace<'a>(
    scopes: &'a ScopeTree,
    node: Node<'_>,
    name: &str,
) -> Option<&'a Binding> {
    let at = node.start_byte() as u32;
    match node.kind() {
        "type_identifier" => scopes.resolve_type(at, name),
        _ => scopes
            .resolve_value(at, name)
            .or_else(|| scopes.resolve_type(at, name)),
    }
}

/// Every plain-name identifier token in the file spelled `name`.
///
/// "Plain name" is [`is_simple_name`]'s filter, the same one the cursor's own
/// token is held to: `other.count` selects a member from whatever `other` is,
/// and is not a candidate, because a coincidentally same-named local must not
/// light up beside it.
///
/// `token` is checked before each node is visited, not only once by the
/// caller: a file's identifier count is exactly the "unit of work
/// proportional to file size" cancellation exists for, and a check only at
/// [`occurrences_at`]'s entry would let a cancelled request still pay for the
/// whole walk before anyone reads its answer.
///
/// A pre-order walk over a tree whose nodes lie in source order visits each
/// one exactly once, in strictly increasing `start_byte` order — which is
/// [`occurrences_at`]'s basis for not sorting or deduplicating what this
/// returns.
fn collect_identifier_tokens<'a>(
    node: Node<'a>,
    document: &TextDocument,
    name: &str,
    out: &mut Vec<Node<'a>>,
    token: &CancellationToken,
) -> Result<(), Cancelled> {
    token.check()?;
    if is_identifier(node) && is_simple_name(node) && text(document, node) == Some(name) {
        out.push(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifier_tokens(child, document, name, out, token)?;
    }
    Ok(())
}

fn from_binding(
    document: &TextDocument,
    tree: &SyntaxTree,
    syntax: &FileSyntax,
    binding: &Binding,
    reference: Span,
) -> Definition {
    let decl = syntax.declaration_at(binding.name_span);
    Definition {
        file: document.id().clone(),
        reference,
        name: binding.name.clone(),
        kind: from_binding_kind(binding.kind),
        span: binding.span,
        name_span: binding.name_span,
        signature: signature_of(document, tree, binding.span),
        doc: decl.and_then(|decl| decl.doc.clone()),
    }
}

/// The identifier token at an offset, if there is one.
pub(crate) fn identifier_at(root: Node<'_>, offset: u32) -> Option<Node<'_>> {
    let offset = offset as usize;
    if let Some(node) = root
        .descendant_for_byte_range(offset, offset)
        .filter(|node| is_identifier(*node))
    {
        return Some(node);
    }
    // A cursor sitting immediately after a name is still on it: that is where it
    // lands when the user has just finished typing the name.
    let before = offset.checked_sub(1)?;
    root.descendant_for_byte_range(before, before)
        .filter(|node| is_identifier(*node))
}

pub(crate) fn is_identifier(node: Node<'_>) -> bool {
    matches!(node.kind(), "identifier" | "type_identifier")
}

/// The plain name at an offset where it stands for a value — `MAX` in
/// `return MAX;` — rather than naming what a declaration declares, an
/// annotation, an element-value key, a label, a lambda's parameter or a
/// `case` constant, and resolved against nothing.
///
/// For the caller that reads a name the scope tree leaves unbound through
/// something else, a static import: a declaration's own name is the one
/// place a value's spelling means no value.
pub fn expression_name_at(
    document: &TextDocument,
    tree: &SyntaxTree,
    offset: u32,
) -> Option<(String, Span)> {
    let identifier = identifier_at(tree.root(), offset).filter(|node| is_expression_name(*node))?;
    Some((
        text(document, identifier)?.to_owned(),
        span(document, identifier),
    ))
}

/// Whether an identifier token is a name in expression position; see
/// [`expression_name_at`].
pub(crate) fn is_expression_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    node.kind() == "identifier"
        && is_simple_name(node)
        && parent.child_by_field_name("name") != Some(node)
        && parent.child_by_field_name("key") != Some(node)
        && !matches!(
            parent.kind(),
            "lambda_expression"
                | "inferred_parameters"
                | "switch_label"
                | "package_declaration"
                | "method_reference"
        )
}

/// Whether the identifier is a plain name rather than something selected from
/// an expression.
///
/// `other.count` and `other.count()` name a member of whatever `other` is, and
/// working that out is type resolution. Answering them from a same-named local
/// would be a guess, so they get no answer at all.
pub(crate) fn is_simple_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return true;
    };
    match parent.kind() {
        "scoped_identifier" | "scoped_type_identifier" => false,
        "field_access" => parent.child_by_field_name("field") != Some(node),
        // ponytail: the name of a call is refused whether or not it has a
        // receiver, because the scope tree binds no methods and picking one by
        // name alone would be wrong the moment the name is overloaded. Neither
        // shape is stuck on that any more: `other.go()` is answered by
        // `javelin_semantic::member_definition_at` and `go()` by
        // `javelin_semantic::call_definition_at`, both before this query runs.
        // What stays refused here is what stays refused there — a call whose
        // name and arity fit two methods that the argument types cannot
        // separate either, where an answer would be a guess. Wire this arm to
        // whatever narrows that further, not to a name lookup.
        "method_invocation" => parent.child_by_field_name("name") != Some(node),
        // Java keeps labels in a namespace of their own (JLS 6.5.1), so
        // `break outer;` never refers to a variable called `outer`. Nothing
        // below distinguishes the two namespaces, so the label is refused here.
        "break_statement" | "continue_statement" => false,
        "labeled_statement" => parent.child(0) != Some(node),
        // `Type::member` — only the left-hand side names something this file
        // could have declared.
        "method_reference" => parent.child(0) == Some(node),
        // `@A(name = …)`'s `name` is an element of `A`, never a variable in
        // scope — `annotation_element_at`'s question.
        "element_value_pair" => parent.child_by_field_name("key") != Some(node),
        _ => true,
    }
}

/// The element `@A(name = …)` names, with the cursor on `name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotationElement {
    /// The annotation's type as written, `A` or `com.acme.A`.
    pub annotation: String,
    /// Where that name starts, which is where it is resolved.
    pub annotation_at: u32,
    pub name: String,
    pub name_span: Span,
}

/// The annotation element whose key the cursor is on, if it is on one.
pub fn annotation_element_at(
    document: &TextDocument,
    tree: &SyntaxTree,
    offset: u32,
) -> Option<AnnotationElement> {
    let key = identifier_at(tree.root(), offset)?;
    let pair = key.parent()?;
    if pair.kind() != "element_value_pair" || pair.child_by_field_name("key") != Some(key) {
        return None;
    }
    let annotation = pair.parent()?.parent()?;
    if annotation.kind() != "annotation" {
        return None;
    }
    let written = annotation.child_by_field_name("name")?;
    Some(AnnotationElement {
        annotation: text(document, written)?.to_owned(),
        annotation_at: span(document, written).start_byte,
        name: text(document, key)?.to_owned(),
        name_span: span(document, key),
    })
}

/// A declaration as written, with any body left off.
///
/// Public because `javelin-semantic` renders the same text for a member it
/// resolved, and two renderings of one declaration would eventually disagree.
pub fn signature_of(document: &TextDocument, tree: &SyntaxTree, span: Span) -> String {
    let end = body_start(tree, span).unwrap_or(span.end_byte);
    document
        .text()
        .get(span.start_byte as usize..end as usize)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn body_start(tree: &SyntaxTree, span: Span) -> Option<u32> {
    let node = tree
        .root()
        .descendant_for_byte_range(span.start_byte as usize, span.end_byte as usize)?;
    Some(node.child_by_field_name("body")?.start_byte() as u32)
}

pub(crate) fn span(document: &TextDocument, node: Node<'_>) -> Span {
    let start_byte = node.start_byte() as u32;
    let end_byte = node.end_byte() as u32;
    Span {
        start_byte,
        end_byte,
        range: javelin_document::Range::new(
            document.position_at(start_byte),
            document.position_at(end_byte),
        ),
    }
}

/// Slicing rather than `utf8_text` so a node whose range the grammar reported
/// oddly yields nothing instead of an error.
pub(crate) fn text<'a>(document: &'a TextDocument, node: Node<'_>) -> Option<&'a str> {
    document.text().get(node.start_byte()..node.end_byte())
}

fn from_binding_kind(kind: BindingKind) -> DefinitionKind {
    match kind {
        BindingKind::LocalVariable => DefinitionKind::LocalVariable,
        BindingKind::Parameter => DefinitionKind::Parameter,
        BindingKind::PatternBinding => DefinitionKind::PatternBinding,
        BindingKind::Field => DefinitionKind::Field,
        BindingKind::TypeParameter => DefinitionKind::TypeParameter,
        BindingKind::TypeDeclaration => DefinitionKind::Type,
    }
}

impl From<DeclKind> for DefinitionKind {
    fn from(kind: DeclKind) -> Self {
        match kind {
            DeclKind::Class
            | DeclKind::Interface
            | DeclKind::Enum
            | DeclKind::Record
            | DeclKind::AnnotationType
            | DeclKind::AnonymousClass => DefinitionKind::Type,
            DeclKind::Method | DeclKind::AnnotationElement => DefinitionKind::Method,
            DeclKind::Constructor => DefinitionKind::Constructor,
            DeclKind::Field | DeclKind::RecordComponent | DeclKind::EnumConstant => {
                DefinitionKind::Field
            }
            DeclKind::Parameter => DefinitionKind::Parameter,
            // Initialiser blocks have no name, so nothing reaches this arm
            // through `declaration_at`; a local is the only other
            // nameless-adjacent case.
            DeclKind::LocalVariable
            | DeclKind::StaticInitializer
            | DeclKind::InstanceInitializer => DefinitionKind::LocalVariable,
        }
    }
}

/// A name completion can offer at a position, and what the editor shows for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    pub kind: DefinitionKind,
    /// The declared type where the declaration has one, otherwise the whole
    /// declaration with any body left off. Always as the author wrote it.
    pub detail: Option<String>,
    /// The `/** … */` above the declaration, verbatim and unparsed.
    pub doc: Option<String>,
}

/// How many candidates one request will produce.
///
/// ponytail: a flat cap rather than ranking. A generated god-class binds tens of
/// thousands of names, and building — let alone serialising — all of them costs
/// more than the whole interactive budget for a popup nobody scrolls. The list
/// is returned incomplete, so the client re-asks as the prefix narrows, which is
/// what makes truncation survivable.
///
/// Truncation is biased twice over: half the cap is reserved for the enclosing
/// types' methods so a class with more fields than the cap still offers any,
/// and within each half it is source order that decides, not relevance. Rank
/// before truncating if a real file is ever seen to lose the candidate the user
/// wanted.
const MAX_CANDIDATES: usize = 500;

/// What can be completed at a byte offset, innermost binding first.
///
/// Candidates are already filtered by the partial identifier the cursor sits
/// at the end of. That filter is here rather than in the caller because each
/// surviving candidate costs a lookup and two allocations, and a file with
/// twenty thousand members would pay all of them to have the caller throw the
/// answer away.
///
/// An empty result is an ordinary answer: the cursor is after a `.`, or nothing
/// this file declares starts with what has been typed. Neither is an error and
/// neither is worth a guess — a member selected from an expression needs the
/// receiver's type, which is `javelin_semantic::member_completions_at`'s question.
pub fn completions_at(
    document: &TextDocument,
    tree: &SyntaxTree,
    scopes: &ScopeTree,
    syntax: &FileSyntax,
    offset: u32,
) -> Vec<Candidate> {
    let offset = offset.min(document.text().len() as u32);
    let Some(word) = word_before(document.text(), offset) else {
        return Vec::new();
    };
    if word.qualified {
        return Vec::new();
    }

    // Methods claim their share of the cap before the bindings spend it: a
    // generated class with more fields than the cap would otherwise offer not
    // one of its methods, which is half of what completion is for.
    let methods: Vec<&Decl> = enclosing_methods(syntax, offset)
        .into_iter()
        .filter(|decl| decl.name.starts_with(word.prefix))
        .take(MAX_CANDIDATES / 2)
        .collect();
    let bindings: Vec<&Binding> = scopes
        .visible_at(offset)
        .into_iter()
        .filter(|binding| binding.name.starts_with(word.prefix))
        .take(MAX_CANDIDATES.saturating_sub(methods.len()))
        .collect();
    if bindings.is_empty() && methods.is_empty() {
        return Vec::new();
    }

    let declarations = declarations_by_name_start(syntax);
    let mut candidates = Vec::with_capacity(bindings.len() + methods.len());
    candidates.extend(bindings.into_iter().map(|binding| {
        let decl = declarations.get(&binding.name_span.start_byte).copied();
        Candidate {
            name: binding.name.clone(),
            kind: from_binding_kind(binding.kind),
            detail: match decl.and_then(|decl| decl.declared_type.clone()) {
                Some(declared) => Some(declared),
                None => non_empty(signature_of(document, tree, binding.span)),
            },
            doc: decl.and_then(|decl| decl.doc.clone()),
        }
    }));
    candidates.extend(methods.into_iter().map(|decl| Candidate {
        name: decl.name.clone(),
        kind: DefinitionKind::from(decl.kind),
        // The return type alone would hide the parameters, which are the whole
        // reason two methods of one name are two candidates.
        detail: non_empty(signature_of(document, tree, decl.span)),
        doc: decl.doc.clone(),
    }));
    candidates
}

/// The identifier characters immediately before an offset, and whether they are
/// selected from something to their left.
struct Word<'a> {
    prefix: &'a str,
    /// `other.cou`, `Type::val` — the name belongs to whatever stands left of
    /// the separator, and nothing in scope here can answer for it.
    qualified: bool,
}

/// `None` if the offset does not fall on a character boundary, which a position
/// off the wire should never produce and which must not panic if it does.
fn word_before(text: &str, offset: u32) -> Option<Word<'_>> {
    let before = text.get(..offset as usize)?;
    let start = before
        .char_indices()
        .rev()
        .find(|(_, character)| !is_identifier_char(*character))
        .map_or(0, |(index, character)| index + character.len_utf8());
    let (left, prefix) = before.split_at(start);
    let left = left.trim_end();
    Some(Word {
        prefix,
        // Whitespace is skipped, because `other.` followed by a newline is still
        // a member selection and dumping every local into that popup is the one
        // thing completion must not do. `int... args` is the exception: the dots
        // there open a parameter, they do not select from anything.
        qualified: !left.ends_with("...") && (left.ends_with('.') || left.ends_with("::")),
    })
}

fn is_identifier_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '$'
}

/// The methods of every type enclosing `offset`, innermost type first.
///
/// The scope tree binds no methods: a call site cannot be resolved to one
/// without knowing the receiver's type, so it never tried. Completion is the one
/// caller that does not need it to — it offers names rather than picking one —
/// so its methods come from the syntax view instead.
///
/// ponytail: every method of an enclosing type, static or not, and nothing
/// inherited. Offering a name that is there to be typed beats withholding it.
/// Both need the type model, which is `javelin_semantic`'s and not this
/// crate's: the upgrade is answering bare-name completion there, as
/// `member_completions_at` already answers a selection.
fn enclosing_methods(syntax: &FileSyntax, offset: u32) -> Vec<&Decl> {
    let mut enclosing = Vec::new();
    let mut level = &syntax.declarations;
    while let Some(decl) = level
        .iter()
        .find(|decl| is_type(decl.kind) && contains(decl.span, offset))
    {
        enclosing.push(decl);
        level = &decl.children;
    }
    enclosing
        .iter()
        .rev()
        .flat_map(|decl| decl.children.iter())
        .filter(|decl| {
            matches!(decl.kind, DeclKind::Method | DeclKind::AnnotationElement)
                && !decl.name.is_empty()
        })
        .collect()
}

/// Every named declaration in the file, by the byte its name starts at.
///
/// One walk per request rather than [`FileSyntax::declaration_at`]'s walk per
/// lookup: a completion answers with hundreds of names, and the two together
/// would be quadratic in the size of the file.
fn declarations_by_name_start(syntax: &FileSyntax) -> HashMap<u32, &Decl> {
    fn walk<'a>(decls: &'a [Decl], out: &mut HashMap<u32, &'a Decl>) {
        for decl in decls {
            if !decl.name.is_empty() {
                out.insert(decl.name_span.start_byte, decl);
            }
            walk(&decl.parameters, out);
            walk(&decl.children, out);
        }
    }
    let mut out = HashMap::new();
    walk(&syntax.declarations, &mut out);
    out
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

fn contains(span: Span, offset: u32) -> bool {
    span.start_byte <= offset && offset <= span.end_byte
}

fn non_empty(text: String) -> Option<String> {
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JavaParser;
    use javelin_document::DocumentId;

    /// A source string and everything built from it, so a test can name an
    /// offset by the text at it instead of counting bytes.
    struct Fixture {
        document: TextDocument,
        tree: SyntaxTree,
        scopes: ScopeTree,
        syntax: FileSyntax,
    }

    fn fixture(source: &str) -> Fixture {
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 1, source);
        let tree = JavaParser::new()
            .expect("grammar loads")
            .parse(&document)
            .expect("a tree");
        let scopes = ScopeTree::build(&document, &tree);
        let syntax = FileSyntax::build(&document, &tree);
        Fixture {
            document,
            tree,
            scopes,
            syntax,
        }
    }

    impl Fixture {
        /// The offset of the first byte of `marker`.
        fn at(&self, marker: &str) -> u32 {
            self.document
                .text()
                .find(marker)
                .unwrap_or_else(|| panic!("no {marker:?} in the fixture")) as u32
        }

        /// The offset of the first byte of the `nth` occurrence of `marker`,
        /// counting from zero.
        fn nth(&self, nth: usize, marker: &str) -> u32 {
            self.document
                .text()
                .match_indices(marker)
                .nth(nth)
                .unwrap_or_else(|| panic!("fewer than {} of {marker:?}", nth + 1))
                .0 as u32
        }

        fn resolve(&self, offset: u32) -> Option<Definition> {
            definition_at(
                &self.document,
                &self.tree,
                &self.scopes,
                &self.syntax,
                offset,
            )
        }

        /// The line and character the definition of the identifier at `offset`
        /// starts at, rendered so a test can state it in one string.
        fn declaration_of(&self, offset: u32) -> String {
            let found = self.resolve(offset).expect("a definition");
            format!(
                "{}:{} {:?} {}",
                found.name_span.range.start.line,
                found.name_span.range.start.character,
                found.kind,
                found.signature
            )
        }
    }

    const SHADOWED: &str = "\
class Shadowing {
    int inMethod(int value) {
        int fromParameter = value;
        {
            int value = 3;
            int fromBlock = value;
        }
        return fromParameter;
    }

    private int value = 1;
}
";

    #[test]
    fn a_name_resolves_to_the_innermost_declaration_that_binds_it() {
        let file = fixture(SHADOWED);
        assert_eq!(
            file.declaration_of(file.nth(1, "value")),
            "1:21 Parameter int value",
            "`fromParameter = value` sees the parameter"
        );
        assert_eq!(
            file.declaration_of(file.nth(3, "value")),
            "4:16 LocalVariable int value = 3;",
            "`fromBlock = value` sees the block local that shadows it"
        );
    }

    #[test]
    fn a_definition_names_the_document_it_was_resolved_in() {
        let file = fixture(SHADOWED);
        // The declaration's own name and a name reached through a binding both
        // travel through the same construction site, so one fixture proves both.
        assert_eq!(
            file.resolve(file.at("inMethod")).expect("the method").file,
            DocumentId::new("file:///A.java")
        );
        assert_eq!(
            file.resolve(file.nth(1, "value"))
                .expect("the parameter")
                .file,
            DocumentId::new("file:///A.java")
        );
    }

    #[test]
    fn a_field_declared_below_its_uses_still_resolves() {
        let file = fixture("class A { int go() { return value; } private int value = 1; }");
        assert_eq!(
            file.declaration_of(file.at("value")),
            "0:49 Field private int value = 1;"
        );
    }

    #[test]
    fn a_declarations_own_name_resolves_to_itself() {
        let file = fixture(SHADOWED);
        let found = file.resolve(file.at("inMethod")).expect("the method");
        assert_eq!(found.kind, DefinitionKind::Method);
        assert_eq!(found.name_span, found.reference);
    }

    #[test]
    fn a_method_is_reachable_at_its_own_name_even_though_nothing_binds_it() {
        let file = fixture("class A { <T> T pick(T first) throws Error { return first; } }");
        assert_eq!(
            file.declaration_of(file.at("pick")),
            "0:16 Method <T> T pick(T first) throws Error"
        );
    }

    #[test]
    fn a_signature_stops_before_the_body_it_introduces() {
        let file = fixture("class Box { void go() { int hidden = 1; } }");
        assert_eq!(file.declaration_of(file.at("Box")), "0:6 Type class Box");
        assert!(
            !file
                .resolve(file.at("go"))
                .expect("go")
                .signature
                .contains("hidden")
        );
    }

    #[test]
    fn a_type_reference_resolves_to_the_type_and_not_to_a_variable_of_that_name() {
        let file = fixture("class A { class Value {} void go() { int Value = 1; Value v; } }");
        assert_eq!(
            file.declaration_of(file.at("Value v")),
            "0:16 Type class Value"
        );
    }

    #[test]
    fn a_name_this_file_does_not_declare_has_no_definition() {
        let file = fixture("import java.util.List;\nclass A { List<String> items; }");
        assert!(file.resolve(file.nth(1, "List")).is_none());
    }

    #[test]
    fn whitespace_and_keywords_have_no_definition() {
        let file = fixture("class A { int x = 1; }");
        assert!(file.resolve(file.at("class")).is_none(), "on `class`");
        assert!(file.resolve(file.at("= 1") + 1).is_none(), "on the space");
        assert!(file.resolve(file.at("int")).is_none(), "on `int`");
    }

    #[test]
    fn a_member_selected_from_an_expression_is_not_answered_from_a_local() {
        let file = fixture(
            "class A { void go(A other) { int count = 1; int c = other.count; other.count(); } }",
        );
        assert!(file.resolve(file.at("count;")).is_none(), "`other.count`");
        assert!(
            file.resolve(file.at("count()")).is_none(),
            "`other.count()`"
        );
        // The receiver itself is a plain name and does resolve.
        assert_eq!(
            file.resolve(file.at("other.count")).expect("other").kind,
            DefinitionKind::Parameter
        );
    }

    #[test]
    fn a_type_named_where_a_variable_could_also_stand_still_resolves() {
        let file = fixture(
            "class A { static class Helper { static void go() {} } void run() { Helper.go(); } }",
        );
        assert_eq!(
            file.resolve(file.at("Helper.go"))
                .expect("the nested type")
                .kind,
            DefinitionKind::Type
        );
    }

    #[test]
    fn only_the_left_of_a_method_reference_is_answered() {
        let file = fixture("class Pick { void go() { int valueOf = 1; var f = Pick::valueOf; } }");
        assert_eq!(
            file.resolve(file.at("Pick::")).expect("the type").kind,
            DefinitionKind::Type
        );
        assert!(file.resolve(file.at("valueOf;")).is_none());
    }

    #[test]
    fn a_label_is_not_answered_from_a_variable_of_the_same_name() {
        let file = fixture(
            "class A { void go() { int outer = 1; outer: while (true) { break outer; } } }",
        );
        assert!(file.resolve(file.at("outer: ")).is_none(), "the label");
        assert!(file.resolve(file.at("outer;")).is_none(), "`break outer`");
        // The local of the same name still resolves where it really is named.
        assert_eq!(
            file.resolve(file.at("outer = 1")).expect("the local").kind,
            DefinitionKind::LocalVariable
        );
    }

    /// Completion offers the enclosing type's methods, so this gap is visible.
    /// Closing it needs overload resolution, which is Phase 2.
    #[test]
    fn an_unqualified_call_to_a_method_this_file_declares_has_no_definition() {
        let file = fixture("class A { void go() {} void run() { go(); } }");
        assert!(file.resolve(file.at("go();")).is_none());
        // Its own declaration still resolves, which is what makes the gap odd.
        assert_eq!(
            file.resolve(file.at("go() {}")).expect("the method").kind,
            DefinitionKind::Method
        );
    }

    /// Each segment of a qualified name names the dotted text through it,
    /// wherever the grammar says a type or a package is meant — and nowhere
    /// else.
    #[test]
    fn a_qualified_names_segment_names_the_text_up_to_it() {
        let file = fixture(
            "package p.q;\nimport java.util.Map;\n@a.Ann class A { java.util.List items; \
             Map.Entry e; Outer<String>.Inner i; void m() { java.util.List.of(); } }",
        );
        let at = |marker: &str| {
            qualified_name_at(&file.document, &file.tree, file.at(marker)).map(|found| found.0)
        };
        assert_eq!(at("List items").as_deref(), Some("java.util.List"));
        assert_eq!(at("util.List items").as_deref(), Some("java.util"));
        assert_eq!(at("Entry").as_deref(), Some("Map.Entry"));
        assert_eq!(at("Map;").as_deref(), Some("java.util.Map"));
        assert_eq!(at("Ann").as_deref(), Some("a.Ann"));
        for refused in ["q;", "Inner", "List.of", "items"] {
            assert_eq!(at(refused), None, "{refused}");
        }
    }

    /// `@Column(name = …)`'s `name` names an element of `Column`, never the
    /// field `name` the annotation sits on.
    #[test]
    fn an_annotation_elements_key_names_the_element_and_no_variable() {
        let file = fixture(
            "package p;\nclass A { @Column(name = \"n\") String name; \
             @a.Map(to = @Inner(depth = name)) int x; void m() { f(name); } }",
        );
        let element =
            |marker: &str| annotation_element_at(&file.document, &file.tree, file.at(marker));
        let found = element("name = ").expect("a key");
        assert_eq!(found.annotation, "Column");
        assert_eq!(found.annotation_at, file.at("Column"));
        assert_eq!(found.name, "name");
        assert_eq!(found.name_span.start_byte, file.at("name = "));
        let qualified = element("to =").expect("a qualified annotation's key");
        assert_eq!(qualified.annotation, "a.Map");
        assert_eq!(
            element("depth")
                .expect("a nested annotation's key")
                .annotation,
            "Inner"
        );
        for refused in ["name)", "name; @", "name); }", "Column"] {
            assert_eq!(element(refused), None, "{refused}");
        }
        assert_eq!(
            name_at(&file.document, &file.tree, file.at("name = ")),
            None
        );
        assert!(name_at(&file.document, &file.tree, file.at("name)")).is_some());
    }

    /// A value's spelling, where it stands for one and nowhere else.
    #[test]
    fn an_expression_name_is_a_name_that_stands_for_a_value() {
        let file = fixture(
            "package p;\n@Ann(key = MAX) class A { int MAX; @Mark void m(int p) { int v = MAX + p; \
             call(MAX); out: for (;;) { break out; } run(x -> x); run((y, z) -> y); \
             run(MAX::size); switch (p) { case LOW: } this.MAX = MAX; } }",
        );
        let at = |marker: &str| {
            expression_name_at(&file.document, &file.tree, file.at(marker)).map(|found| found.0)
        };
        assert_eq!(at("MAX)").as_deref(), Some("MAX"), "an element's value");
        assert_eq!(at("MAX +").as_deref(), Some("MAX"));
        assert_eq!(at("MAX);").as_deref(), Some("MAX"), "an argument");
        assert_eq!(
            at("MAX; } }").as_deref(),
            Some("MAX"),
            "an assignment's right side"
        );
        for refused in [
            "Ann",
            "key",
            "A {",
            "MAX; @Mark",
            "m(",
            "p)",
            "v =",
            "call",
            "out:",
            "out;",
            "x ->",
            "LOW",
            "MAX =",
            "p;",
            "Mark",
            "y, z",
            "MAX::",
        ] {
            assert_eq!(at(refused), None, "{refused}");
        }
    }

    #[test]
    fn a_qualified_type_name_is_not_answered_from_its_last_segment() {
        let file = fixture("class List {}\nclass A { java.util.List items; }");
        assert!(file.resolve(file.at("List items")).is_none());
    }

    #[test]
    fn a_cursor_just_past_the_end_of_a_name_is_still_on_it() {
        let file = fixture("class A { int x = 1; int y = x; }");
        let past = file.at("x;") + 1;
        assert_eq!(
            file.resolve(past).expect("still on `x`").kind,
            DefinitionKind::Field
        );
    }

    #[test]
    fn an_offset_past_the_end_of_the_file_answers_instead_of_panicking() {
        let file = fixture("class A {}");
        assert!(file.resolve(9_000).is_none());
    }

    #[test]
    fn a_half_typed_file_still_resolves_what_did_parse() {
        let file = fixture("class A { int x = 1; void go() { int y = x; ");
        assert_eq!(
            file.resolve(file.at("x; ")).expect("the field").kind,
            DefinitionKind::Field
        );
    }

    #[test]
    fn a_doc_comment_above_a_declaration_travels_with_it() {
        let file = fixture(
            "class A {\n  /** How many. */\n  private int count;\n  int go() { return count; }\n}",
        );
        let found = file.resolve(file.at("count; }")).expect("the field");
        assert_eq!(found.doc.as_deref(), Some("/** How many. */"));
        assert_eq!(found.signature, "private int count;");
    }

    #[test]
    fn an_undocumented_declaration_carries_no_doc() {
        let file = fixture("class A {\n  /* not a doc. */\n  private int count;\n}");
        assert_eq!(file.resolve(file.at("count")).expect("the field").doc, None);
    }

    /// Known limitation of the scope builder, not of this module: an
    /// `instanceof` binding's declaration span is collapsed onto its name, so
    /// there is no `String s` to show. Pinned here so widening the span there
    /// shows up as a change in what hover renders.
    #[test]
    fn an_instanceof_binding_reads_as_its_name_alone() {
        let file = fixture(
            "class A { void go(Object o) { if (o instanceof String s) { int n = s.length(); } } }",
        );
        let found = file.resolve(file.at("s.length")).expect("the binding");
        assert_eq!(found.kind, DefinitionKind::PatternBinding);
        assert_eq!(found.signature, "s");
    }

    impl Fixture {
        fn candidates(&self, offset: u32) -> Vec<Candidate> {
            completions_at(
                &self.document,
                &self.tree,
                &self.scopes,
                &self.syntax,
                offset,
            )
        }

        /// The popup at an offset, one candidate per line, so the whole of it
        /// reads as one string.
        fn completions(&self, offset: u32) -> String {
            self.candidates(offset)
                .iter()
                .map(|candidate| {
                    format!(
                        "{:?} {}: {}",
                        candidate.kind,
                        candidate.name,
                        candidate.detail.as_deref().unwrap_or("?")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        fn names_at(&self, offset: u32) -> Vec<String> {
            self.candidates(offset)
                .into_iter()
                .map(|candidate| candidate.name)
                .collect()
        }
    }

    /// `// HERE` marks the cursor. Tests that need something typed there replace
    /// it, so every one of them shares the same surrounding declarations.
    const COMPLETING: &str = "\
class Completing {
    /** How many. */
    private int count;

    int add(int by) {
        int running = count;
        // HERE
        int later = 2;
        return running;
    }

    void other() {
    }
}
";

    fn typing(what: &str) -> Fixture {
        fixture(&COMPLETING.replace("// HERE", what))
    }

    #[test]
    fn locals_parameters_and_the_enclosing_types_members_are_all_offered() {
        let file = fixture(COMPLETING);
        assert_eq!(
            file.completions(file.at("// HERE")),
            "\
LocalVariable running: int
Parameter by: int
Field count: int
Type Completing: class Completing
Method add: int add(int by)
Method other: void other()"
        );
    }

    #[test]
    fn a_documented_member_is_offered_with_its_doc_comment() {
        let file = fixture(COMPLETING);
        let candidates = file.candidates(file.at("// HERE"));
        let documented: Vec<_> = candidates
            .iter()
            .filter(|candidate| candidate.doc.is_some())
            .map(|candidate| (candidate.name.as_str(), candidate.doc.as_deref()))
            .collect();
        assert_eq!(documented, vec![("count", Some("/** How many. */"))]);
    }

    #[test]
    fn a_local_declared_below_the_cursor_is_not_offered() {
        let file = fixture(COMPLETING);
        assert!(!file.names_at(file.at("// HERE")).contains(&"later".into()));
        assert!(
            file.names_at(file.at("return running"))
                .contains(&"later".into()),
            "and is offered once the cursor is past it"
        );
    }

    #[test]
    fn a_shadowed_name_is_offered_once_and_it_is_the_innermost_that_wins() {
        let file = fixture(SHADOWED);
        assert_eq!(
            file.completions(file.at("int fromBlock")),
            "\
LocalVariable value: int
LocalVariable fromParameter: int
Type Shadowing: class Shadowing
Method inMethod: int inMethod(int value)"
        );
    }

    #[test]
    fn only_the_names_starting_with_what_has_been_typed_are_offered() {
        let file = typing("int x = co;");
        assert_eq!(file.names_at(file.at("co;") + 2), vec!["count".to_owned()]);
    }

    #[test]
    fn a_prefix_nothing_in_the_file_starts_with_offers_nothing() {
        let file = typing("int x = zz;");
        assert!(file.names_at(file.at("zz;") + 2).is_empty());
    }

    #[test]
    fn a_member_selected_from_an_expression_is_not_completed_from_what_is_in_scope() {
        let bare = typing("int x = running.;");
        assert!(
            bare.names_at(bare.at("running.;") + 8).is_empty(),
            "immediately after `.`"
        );

        let partial = typing("int x = running.co;");
        assert!(
            partial.names_at(partial.at("running.co;") + 10).is_empty(),
            "after `.` with part of a member name typed"
        );

        let reference = typing("var f = Completing::co;");
        assert!(
            reference
                .names_at(reference.at("Completing::co;") + 14)
                .is_empty(),
            "after `::`"
        );
    }

    #[test]
    fn a_gap_between_the_dot_and_the_cursor_is_still_a_member_selection() {
        let spaced = typing("int x = running. c;");
        assert!(
            spaced.names_at(spaced.at("c;") + 1).is_empty(),
            "a space between the `.` and the name"
        );

        let wrapped = typing("int x = running.\n            c;");
        assert!(
            wrapped.names_at(wrapped.at("c;") + 1).is_empty(),
            "a line break between the `.` and the name"
        );
    }

    /// The dots of a varargs parameter are the one `...` that selects nothing,
    /// so the cursor inside the parameter's own name must still be completed.
    #[test]
    fn a_varargs_parameter_is_not_mistaken_for_a_member_selection() {
        let file = fixture("class A { void go(int... values) {} }");
        assert_eq!(
            file.names_at(file.at("values") + 3),
            vec!["values".to_owned()]
        );
    }

    #[test]
    fn the_enclosing_types_methods_survive_a_cap_the_fields_alone_would_exhaust() {
        let mut source = String::from("class Big {\n");
        for field in 0..MAX_CANDIDATES {
            source.push_str(&format!("    int field{field};\n"));
        }
        source.push_str("    void method() { int x = 1; }\n}\n");

        let file = fixture(&source);
        let names = file.names_at(file.at("int x"));
        assert!(names.len() <= MAX_CANDIDATES, "{} offered", names.len());
        assert!(
            names.contains(&"method".to_owned()),
            "the method is offered"
        );
    }

    #[test]
    fn a_nested_types_methods_are_offered_alongside_the_enclosing_types() {
        let file = fixture(
            "class Outer { void outerOne() {} class Inner { void innerOne() { int x = 1; } } }",
        );
        let names = file.names_at(file.at("int x"));
        assert!(names.contains(&"innerOne".to_owned()), "{names:?}");
        assert!(names.contains(&"outerOne".to_owned()), "{names:?}");
    }

    #[test]
    fn a_method_of_a_type_the_cursor_is_not_inside_is_not_offered() {
        let file = fixture("class A { void go() { int x = 1; } }\nclass B { void hidden() {} }");
        assert!(
            !file
                .names_at(file.at("int x"))
                .contains(&"hidden".to_owned())
        );
    }

    #[test]
    fn nothing_the_file_only_imports_is_offered() {
        let file = fixture("import java.util.List;\nclass A { void go() { int x = 1; } }");
        assert!(!file.names_at(file.at("int x")).contains(&"List".to_owned()));
    }

    #[test]
    fn a_half_typed_file_still_completes_what_did_parse() {
        let file = fixture("class A { int count = 1; void go() { int x = co");
        assert_eq!(
            file.names_at(file.document.text().len() as u32),
            vec!["count".to_owned()]
        );
    }

    #[test]
    fn an_offset_past_the_end_of_the_file_completes_instead_of_panicking() {
        let file = fixture("class A { int x = 1; }");
        assert!(!file.names_at(9_000).is_empty());
    }

    #[test]
    fn a_multibyte_prefix_is_scanned_by_characters_rather_than_by_bytes() {
        let file = fixture("class A { int ünlü = 1; void go() { int x = ün; } }");
        assert_eq!(file.names_at(file.at("ün;") + 3), vec!["ünlü".to_owned()]);
    }

    #[test]
    fn a_multibyte_identifier_reports_utf16_columns() {
        let file = fixture("class A { int café = 1; int b = café; }");
        let found = file.resolve(file.nth(1, "café")).expect("the field");
        assert_eq!(found.name_span.range.start.character, 14);
        assert_eq!(found.reference.range.start.character, 32);
    }

    impl Fixture {
        fn occurrences(&self, offset: u32) -> Vec<Span> {
            occurrences_at(
                &self.document,
                &self.tree,
                &self.scopes,
                offset,
                &CancellationToken::new(),
            )
            .expect("not cancelled")
        }

        fn occurrence_starts(&self, offset: u32) -> Vec<u32> {
            self.occurrences(offset)
                .into_iter()
                .map(|found| found.start_byte)
                .collect()
        }
    }

    #[test]
    fn every_use_of_a_local_is_an_occurrence_including_its_declaration() {
        let file = fixture("class A { void go() { int x = 1; int y = x; int z = x + x; } }");
        let expected: Vec<u32> = (0..4).map(|n| file.nth(n, "x")).collect();
        assert_eq!(
            file.occurrence_starts(file.at("x = 1")),
            expected,
            "the declaration and its three later uses, nothing else"
        );
    }

    #[test]
    fn a_shadowed_local_does_not_light_up_the_one_it_shadows() {
        let file = fixture(SHADOWED);
        assert_eq!(
            file.occurrence_starts(file.nth(0, "value")),
            vec![file.nth(0, "value"), file.nth(1, "value")],
            "the parameter and its one use, not the block local that shadows it nor the field"
        );
        assert_eq!(
            file.occurrence_starts(file.nth(2, "value")),
            vec![file.nth(2, "value"), file.nth(3, "value")],
            "the block local that shadows the parameter and its one use, nothing outside the block"
        );
    }

    #[test]
    fn the_same_name_in_two_sibling_methods_does_not_merge() {
        let file = fixture(
            "class A {\n  void one() { int x = 1; int y = x; }\n  void two() { int x = 2; int z = x; }\n}",
        );
        assert_eq!(
            file.occurrence_starts(file.nth(0, "x")),
            vec![file.nth(0, "x"), file.nth(1, "x")],
            "`one`'s local and its use only"
        );
        assert_eq!(
            file.occurrence_starts(file.nth(2, "x")),
            vec![file.nth(2, "x"), file.nth(3, "x")],
            "`two`'s local and its use only, a different binding of the same name"
        );
    }

    #[test]
    fn a_field_and_a_parameter_of_the_same_name_are_different_occurrences() {
        let file = fixture(
            "class A {\n  private int value;\n  int get() { return value; }\n  void go(int value) { value = 2; }\n}",
        );
        assert_eq!(
            file.occurrence_starts(file.nth(0, "value")),
            vec![file.nth(0, "value"), file.nth(1, "value")],
            "the field's own declaration and its use in `get`"
        );
        assert_eq!(
            file.occurrence_starts(file.nth(2, "value")),
            vec![file.nth(2, "value"), file.nth(3, "value")],
            "the parameter's own declaration and its use in `go`, not the field"
        );
    }

    #[test]
    fn a_type_parameter_is_highlighted_across_the_method_that_declares_it() {
        let file = fixture("class A { <T> T pick(T first, T second) { return first; } }");
        let expected: Vec<u32> = (0..4).map(|n| file.nth(n, "T")).collect();
        assert_eq!(
            file.occurrence_starts(file.at("T pick")),
            expected,
            "the declaration, the return type and both parameter types"
        );
    }

    #[test]
    fn a_name_the_scope_tree_does_not_bind_answers_no_occurrences() {
        let file = fixture("import java.util.List;\nclass A { List<String> items; }");
        assert!(file.occurrences(file.nth(1, "List")).is_empty());
    }

    #[test]
    fn a_method_name_answers_no_occurrences_because_scopes_do_not_bind_one() {
        let file = fixture("class A { void go() { go(); } }");
        assert!(
            file.occurrences(file.at("go() {")).is_empty(),
            "the scope tree binds no methods, so the declaration's own name resolves to nothing"
        );
    }

    /// Pins the contract stated on `occurrences_at`'s own doc — sorted by
    /// start offset, no duplicates — rather than proving today's
    /// implementation can violate it: it cannot, because a pre-order walk
    /// over a tree laid out in source order already visits every node once,
    /// in strictly increasing `start_byte` order, and `occurrences_at` does
    /// not sort or deduplicate on top of that (removing those calls and
    /// re-running this test is how that was checked). What this test is for
    /// is the day something *does* threaten the order — the member walk
    /// Phase 3b's roadmap plans would merge a second, differently-ordered
    /// source into this list — so it has something to fail against instead
    /// of silently shipping an unsorted or duplicated answer.
    #[test]
    fn occurrences_come_back_sorted_and_without_duplicates() {
        // The field is declared after both of its uses: this exercises "sorted
        // by start offset", not "declaration before its uses".
        let file = fixture("class A { int go() { return value + value; } private int value = 1; }");
        let starts = file.occurrence_starts(file.at("value + value"));
        assert_eq!(starts.len(), 3, "the two uses and the declaration");
        assert!(
            starts.windows(2).all(|pair| pair[0] < pair[1]),
            "occurrences come back strictly increasing by start offset, which rules out a \
             duplicate as well as disorder: {starts:?}"
        );
    }

    #[test]
    fn a_cancelled_token_refuses_before_resolving_anything() {
        let file = fixture("class A { void go() { int x = 1; int y = x; } }");
        let token = CancellationToken::new();
        token.cancel();
        let result = occurrences_at(
            &file.document,
            &file.tree,
            &file.scopes,
            file.at("x = 1"),
            &token,
        );
        assert_eq!(result, Err(Cancelled));
    }

    /// The scan is the part of this query whose cost is proportional to file
    /// size, so the check that matters is the one inside it — not only the one
    /// at [`occurrences_at`]'s own entry. Calling the scan directly, bypassing
    /// that entry check entirely, proves there is a check inside
    /// `collect_identifier_tokens` itself: if the token were only threaded
    /// through for `occurrences_at`'s own precondition, this call would ignore
    /// it and walk the whole tree regardless. It does not by itself prove
    /// every later recursive call polls too — `token.check()?` being the first
    /// statement of every call is what makes that true, and is confirmed by
    /// reading the function rather than by this one pre-cancelled call.
    #[test]
    fn a_cancelled_token_stops_the_identifier_scan_before_it_visits_a_node() {
        let mut source = String::from("class A {\n");
        for n in 0..2_000 {
            source.push_str(&format!("    int value{n} = {n};\n"));
        }
        source.push_str("}\n");
        let file = fixture(&source);

        let token = CancellationToken::new();
        token.cancel();
        let mut out = Vec::new();
        let result =
            collect_identifier_tokens(file.tree.root(), &file.document, "value0", &mut out, &token);

        assert_eq!(result, Err(Cancelled));
        assert!(
            out.is_empty(),
            "a cancelled token stops the scan at the root rather than finishing it and \
             discarding the answer afterwards"
        );
    }
}
