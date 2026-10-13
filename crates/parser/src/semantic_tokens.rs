//! Semantic token classification: what a span of source *is*, not how LSP
//! encodes it.
//!
//! This crate is the only one allowed to know tree-sitter exists, so the walk
//! that visits every node lives here, the same way [`crate::folding_ranges`],
//! [`crate::selection_ranges`] and [`crate::occurrences_at`] do. Turning a
//! sorted run of [`SemanticToken`]s into LSP's relative five-integer wire
//! encoding is `javelin-server`'s job — that encoding is purely a transport
//! concern, and this module does not know LSP exists.
//!
//! Two sources decide what a token is, and nothing guesses (Ruling 5). A
//! comment, a string or text-block literal, a number-shaped literal
//! (including `true`, `false` and `null` — a separate type for each buys a
//! reviewer nothing) and a Java keyword are read straight off the tree: their
//! kind is syntax, not semantics. An identifier is the one case that needs
//! [`ScopeTree`]: it is resolved at its own offset, exactly the way
//! [`crate::definition_at`] resolves the identifier under the cursor, and an
//! identifier that resolves to nothing — a member selected from an
//! expression, the name of a call, a label, a name the classpath or an import
//! would settle — emits no token at all rather than a guessed one. A name
//! written in a `package` or `import` declaration is the one place a name is
//! unambiguous without resolving anything, so it emits `namespace` directly.

use javelin_core::{CancellationToken, Cancelled};
use javelin_document::{Position, TextDocument};
use tree_sitter::Node;

use crate::SyntaxTree;
use crate::queries::is_simple_name;
use crate::scopes::{BindingKind, ScopeTree};
use crate::syntax::{DeclKind, FileSyntax, Span};

/// What a token is. Nothing here reads the discriminant order — the legend
/// the client is told and the index this crate's caller assigns each variant
/// are both `javelin-server`'s, kept single-sourced in
/// `crates/server/src/handlers/semantic_tokens.rs` so the wire index and the
/// advertised legend cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SemanticTokenKind {
    Namespace,
    Class,
    Interface,
    Enum,
    TypeParameter,
    Parameter,
    Variable,
    Property,
    Keyword,
    Comment,
    String,
    Number,
}

/// One classified span of source, still in absolute UTF-16 line/character —
/// not yet LSP's relative encoding, which is the caller's to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticToken {
    pub line: u32,
    pub start_character: u32,
    /// In UTF-16 code units, matching `start_character`.
    pub length: u32,
    pub kind: SemanticTokenKind,
}

/// Every semantic token in the file, in position order.
///
/// The most nodes any handler in this server visits: every token of the file
/// is inspected once, so `token` is checked once per node the walk descends
/// into and again per token it emits — a file large enough to make this walk
/// slow is exactly the file a keystroke cancels it on.
///
/// Returned in strictly increasing `(line, start_character)` order without an
/// explicit sort: children are visited through [`Node::children`], which
/// walks a parse tree's children in source order, and nothing here descends
/// into the children of a node it has already emitted a token for — the same
/// pre-order-visits-once-in-order basis [`crate::occurrences_at`]'s own doc
/// argues for [`crate::occurrences_at`]'s list.
pub fn semantic_tokens(
    document: &TextDocument,
    tree: &SyntaxTree,
    scopes: &ScopeTree,
    syntax: &FileSyntax,
    token: &CancellationToken,
) -> Result<Vec<SemanticToken>, Cancelled> {
    semantic_tokens_in(document, tree, scopes, syntax, 0..u32::MAX, token)
}

/// [`semantic_tokens`] for the nodes that meet the bytes `within`, and
/// nothing else: the same walk, skipping a subtree that lies wholly outside,
/// so the cost is the range's and not the file's — bar a scan of the
/// siblings beside the path down to it. A token straddling the range's edge
/// is emitted on the lines the range meets, so a file-long comment costs a
/// screen; an empty range emits nothing.
pub fn semantic_tokens_in(
    document: &TextDocument,
    tree: &SyntaxTree,
    scopes: &ScopeTree,
    syntax: &FileSyntax,
    within: std::ops::Range<u32>,
    token: &CancellationToken,
) -> Result<Vec<SemanticToken>, Cancelled> {
    token.check()?;
    let mut out = Vec::new();
    if within.is_empty() {
        return Ok(out);
    }
    let lines = document.position_at(within.start).line
        ..=document.position_at(within.end.saturating_sub(1)).line;
    let walker = Walker {
        document,
        scopes,
        syntax,
        within,
        lines,
        token,
    };
    walker.walk(tree.root(), false, &mut out)?;
    Ok(out)
}

/// What every step of the walk reads and nothing changes.
struct Walker<'a> {
    document: &'a TextDocument,
    scopes: &'a ScopeTree,
    syntax: &'a FileSyntax,
    within: std::ops::Range<u32>,
    /// The lines `within` meets, which a multi-line token is cut to.
    lines: std::ops::RangeInclusive<u32>,
    token: &'a CancellationToken,
}

/// What a node's own kind decides, before any identifier is resolved.
enum Classification {
    /// A leaf whose kind alone determines its token, or an identifier inside
    /// a `package`/`import` name, which resolves to nothing but is still
    /// unambiguous.
    Emit(SemanticTokenKind),
    /// An identifier or type identifier outside a qualified name: resolve it
    /// against the scope tree before deciding.
    Identifier,
    /// Not a token in its own right; walk its children.
    Recurse,
}

fn classify(node: Node<'_>, in_qualified_name: bool) -> Classification {
    use SemanticTokenKind::*;
    match node.kind() {
        "line_comment" | "block_comment" => Classification::Emit(Comment),
        // Text-block literals have no node kind of their own in this
        // grammar: a multi-line `string_literal` *is* a text block, so the
        // line-splitting every multi-line token needs (below) covers both
        // without a separate case.
        "string_literal" => Classification::Emit(String),
        "character_literal"
        | "decimal_integer_literal"
        | "hex_integer_literal"
        | "octal_integer_literal"
        | "binary_integer_literal"
        | "decimal_floating_point_literal"
        | "hex_floating_point_literal"
        | "true"
        | "false"
        | "null_literal" => Classification::Emit(Number),
        "identifier" | "type_identifier" if in_qualified_name => Classification::Emit(Namespace),
        "identifier" | "type_identifier" => Classification::Identifier,
        // `this`, `super`, `boolean` and `void` are reserved words the
        // grammar gives their own named node kind rather than an anonymous
        // token — `int`, `char` and the rest arrive as anonymous children of
        // `integral_type`/`floating_point_type` and are caught by
        // `is_keyword_token` once the walk recurses into those wrappers.
        "this" | "super" | "boolean_type" | "void_type" => Classification::Emit(Keyword),
        kind if is_keyword_token(kind) => Classification::Emit(Keyword),
        _ => Classification::Recurse,
    }
}

/// Every anonymous token in the grammar that spells a Java reserved or
/// contextual word. Punctuation and operators are anonymous too, but none of
/// them spells a word, so they never match here — `non-sealed` is the one
/// entry that is not a bare alphabetic run, and it is a single token in the
/// grammar rather than three.
fn is_keyword_token(kind: &str) -> bool {
    matches!(
        kind,
        "abstract"
            | "assert"
            | "break"
            | "byte"
            | "case"
            | "catch"
            | "char"
            | "class"
            | "continue"
            | "default"
            | "do"
            | "double"
            | "else"
            | "enum"
            | "exports"
            | "extends"
            | "final"
            | "finally"
            | "float"
            | "for"
            | "if"
            | "implements"
            | "import"
            | "instanceof"
            | "int"
            | "interface"
            | "long"
            | "module"
            | "native"
            | "new"
            | "non-sealed"
            | "open"
            | "opens"
            | "package"
            | "permits"
            | "private"
            | "protected"
            | "provides"
            | "public"
            | "record"
            | "requires"
            | "return"
            | "sealed"
            | "short"
            | "static"
            | "strictfp"
            | "switch"
            | "synchronized"
            | "throw"
            | "throws"
            | "to"
            | "transient"
            | "transitive"
            | "try"
            | "uses"
            | "volatile"
            | "when"
            | "while"
            | "with"
            | "yield"
    )
}

impl Walker<'_> {
    fn walk(
        &self,
        node: Node<'_>,
        in_qualified_name: bool,
        out: &mut Vec<SemanticToken>,
    ) -> Result<(), Cancelled> {
        let Walker {
            document,
            scopes,
            syntax,
            within,
            lines,
            token,
        } = self;
        token.check()?;
        if node.start_byte() as u32 >= within.end || (node.end_byte() as u32) <= within.start {
            return Ok(());
        }
        match classify(node, in_qualified_name) {
            Classification::Emit(kind) => return push(document, node, kind, lines, out, token),
            Classification::Identifier => {
                if let Some(kind) = identifier_kind(document, scopes, syntax, node) {
                    push(document, node, kind, lines, out, token)?;
                }
                return Ok(());
            }
            Classification::Recurse => {}
        }
        // Only a `package_declaration` or an `import_declaration` opens a
        // qualified-name context, and every identifier under one — including a
        // `scoped_identifier`'s own segments — is a namespace name, never a use
        // this file's scope tree could resolve.
        let nested = in_qualified_name
            || matches!(node.kind(), "package_declaration" | "import_declaration");
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(child, nested, out)?;
        }
        Ok(())
    }
}

/// What the identifier at `node`'s own offset means, per the scope tree.
///
/// `is_simple_name` is the same guard [`crate::definition_at`] and
/// [`crate::occurrences_at`] hold every identifier to: `other.count`'s
/// `count`, a call's own name, a label, and a `scoped_identifier`'s segments
/// are all refused here for the reason they are refused there — resolving
/// them against the scope tree at their own offset can find a same-named
/// binding that means something else entirely, and a token painted from that
/// guess is worse than no token (Ruling 5).
fn identifier_kind(
    document: &TextDocument,
    scopes: &ScopeTree,
    syntax: &FileSyntax,
    node: Node<'_>,
) -> Option<SemanticTokenKind> {
    if !is_simple_name(node) {
        return None;
    }
    let name = document.text().get(node.start_byte()..node.end_byte())?;
    let at = node.start_byte() as u32;
    // The same ambiguous-name order `definition_at` and `occurrences_at` ask
    // the scope tree in (JLS 6.5.2): a `type_identifier` only ever means a
    // type, and a plain `identifier` is a value if one is in scope and a type
    // otherwise.
    let binding = match node.kind() {
        "type_identifier" => scopes.resolve_type(at, name),
        _ => scopes
            .resolve_value(at, name)
            .or_else(|| scopes.resolve_type(at, name)),
    }?;
    Some(match binding.kind {
        BindingKind::LocalVariable | BindingKind::PatternBinding => SemanticTokenKind::Variable,
        BindingKind::Parameter => SemanticTokenKind::Parameter,
        BindingKind::Field => SemanticTokenKind::Property,
        BindingKind::TypeParameter => SemanticTokenKind::TypeParameter,
        BindingKind::TypeDeclaration => type_declaration_kind(syntax, binding.name_span),
    })
}

/// `class`, `interface` or `enum`, read off the `Decl` a type binding's name
/// span came from. Every other type-declaration kind — record, annotation
/// type, anonymous class — falls back to `class`, per the brief: none of them
/// has a token type of its own in the legend.
fn type_declaration_kind(syntax: &FileSyntax, name_span: Span) -> SemanticTokenKind {
    match syntax.declaration_at(name_span).map(|decl| decl.kind) {
        Some(DeclKind::Interface) => SemanticTokenKind::Interface,
        Some(DeclKind::Enum) => SemanticTokenKind::Enum,
        _ => SemanticTokenKind::Class,
    }
}

/// Emit `node` as one token per line it covers.
///
/// A token that spans more than one line is not representable in LSP's
/// encoding, so a multi-line node — a block comment or a text-block
/// `string_literal` — is split at line boundaries: one token running from the
/// line's first covered character to its last, for every line in between.
/// Every other node this module emits is single-line by construction, and the
/// loop below still produces exactly one token for it, so there is only one
/// code path rather than a single-line case and a multi-line case that could
/// drift apart.
fn push(
    document: &TextDocument,
    node: Node<'_>,
    kind: SemanticTokenKind,
    lines: &std::ops::RangeInclusive<u32>,
    out: &mut Vec<SemanticToken>,
    token: &CancellationToken,
) -> Result<(), Cancelled> {
    let start = document.position_at(node.start_byte() as u32);
    let end = document.position_at(node.end_byte() as u32);
    for line in start.line.max(*lines.start())..=end.line.min(*lines.end()) {
        token.check()?;
        let start_character = if line == start.line {
            start.character
        } else {
            0
        };
        let end_character = if line == end.line {
            end.character
        } else {
            end_of_line_character(document, line)
        };
        // A line the node covers wholly but which is itself empty — a blank
        // line inside a block comment — contributes nothing to highlight.
        if end_character > start_character {
            out.push(SemanticToken {
                line,
                start_character,
                length: end_character - start_character,
                kind,
            });
        }
    }
    Ok(())
}

/// The UTF-16 character one past the last one a line holds, not counting its
/// line terminator.
///
/// `LineIndex::offset` already clamps a character past a line's end to the
/// line's own content end, which is exactly "the whole line" a token covering
/// a middle line of a multi-line node needs.
fn end_of_line_character(document: &TextDocument, line: u32) -> u32 {
    let end_of_line = document.offset_at(Position::new(line, u32::MAX));
    document.position_at(end_of_line).character
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JavaParser;
    use javelin_document::DocumentId;

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
        fn tokens_in(&self, within: std::ops::Range<u32>) -> Vec<SemanticToken> {
            semantic_tokens_in(
                &self.document,
                &self.tree,
                &self.scopes,
                &self.syntax,
                within,
                &CancellationToken::new(),
            )
            .expect("not cancelled")
        }

        fn tokens(&self) -> Vec<SemanticToken> {
            semantic_tokens(
                &self.document,
                &self.tree,
                &self.scopes,
                &self.syntax,
                &CancellationToken::new(),
            )
            .expect("not cancelled")
        }
    }

    #[test]
    fn the_hyphenated_non_sealed_modifier_is_a_keyword_like_every_other() {
        // The one reserved word the grammar spells with a hyphen, and so the
        // one that a list built by eyeballing an alphabetic-token dump skips.
        let fixture = fixture("public non-sealed class Foo extends Bar {}\n");
        let tokens = fixture.tokens();
        let modifier = tokens
            .iter()
            .find(|t| t.start_character == "public ".len() as u32)
            .unwrap_or_else(|| panic!("no token where `non-sealed` starts: {tokens:?}"));
        assert_eq!(modifier.kind, SemanticTokenKind::Keyword);
        assert_eq!(modifier.length, "non-sealed".len() as u32);
    }

    #[test]
    fn a_block_comment_spanning_three_lines_becomes_three_tokens() {
        let fixture = fixture("/* one\ntwo\nthree */\nclass Foo {}\n");
        let tokens = fixture.tokens();
        let comments: Vec<&SemanticToken> = tokens
            .iter()
            .filter(|t| t.kind == SemanticTokenKind::Comment)
            .collect();
        assert_eq!(comments.len(), 3, "one token per covered line: {tokens:?}");
        assert_eq!(comments[0].line, 0);
        assert_eq!(comments[1].line, 1);
        assert_eq!(comments[2].line, 2);
    }

    #[test]
    fn an_identifier_the_scope_tree_does_not_bind_emits_no_token() {
        // `other` is never declared, so `other.count` names nothing the
        // scope tree binds — neither `other` (an unresolved receiver) nor
        // `count` (a call's own name, refused by `is_simple_name` regardless).
        // `m`, the enclosing method's own name, is refused for the same
        // reason: the scope tree binds no methods at all (`BindingKind` has
        // no `Method` variant), so resolving it finds nothing either.
        //
        // Asserted as the exact token sequence rather than "no Variable or
        // Property token anywhere": that looser assertion would not notice a
        // bug that classified `other` or `count` as, say, `Class` or
        // `Namespace` instead of dropping them — only `class`, `Foo` and
        // `void` may ever appear here.
        let fixture = fixture("class Foo { void m() { other.count(); } }\n");
        let tokens = fixture.tokens();
        assert_eq!(
            tokens.len(),
            3,
            "class, Foo and void only — m, other and count all resolve to \
             nothing and must be silently dropped: {tokens:?}"
        );
        assert_eq!(
            tokens[0].kind,
            SemanticTokenKind::Keyword,
            "`class`: {tokens:?}"
        );
        assert_eq!(
            tokens[1].kind,
            SemanticTokenKind::Class,
            "`Foo`: {tokens:?}"
        );
        assert_eq!(
            tokens[2].kind,
            SemanticTokenKind::Keyword,
            "`void`: {tokens:?}"
        );
    }

    #[test]
    fn a_local_a_parameter_and_a_field_get_three_different_token_types() {
        let source = "class Foo {\n\
             int field;\n\
             void m(int parameter) {\n\
                 int local = parameter;\n\
                 field = local;\n\
             }\n\
         }\n";
        let fixture = fixture(source);
        let tokens = fixture.tokens();
        assert!(
            tokens.iter().any(|t| t.kind == SemanticTokenKind::Property),
            "the field use: {tokens:?}"
        );
        assert!(
            tokens
                .iter()
                .any(|t| t.kind == SemanticTokenKind::Parameter),
            "the parameter declaration: {tokens:?}"
        );
        assert!(
            tokens.iter().any(|t| t.kind == SemanticTokenKind::Variable),
            "the local variable declaration: {tokens:?}"
        );
    }

    /// A range's tokens are exactly the whole file's on its lines — the one
    /// walk, skipping what does not meet the range — and a token straddling
    /// its edge is cut to them.
    #[test]
    fn a_ranges_tokens_are_the_whole_files_on_its_lines() {
        let source = "package p;\n/** One\n * two */\nclass A<T> {\n  int count;\n  \
                      String m(String s) {\n    int n = count;\n    return \"\"\"\n      x\n      \"\"\";\n  }\n}\n";
        let fixture = fixture(source);
        let full = fixture.tokens();
        let mut start = 0u32;
        for (line, text) in source.split_inclusive('\n').enumerate() {
            let end = start + text.len() as u32;
            let ranged = fixture.tokens_in(start..end);
            let on_line: Vec<_> = full
                .iter()
                .filter(|t| t.line == line as u32)
                .cloned()
                .collect();
            assert_eq!(ranged, on_line, "line {line}");
            start = end;
        }
        // The doc comment's last line does not bring its first with it.
        let second = source.find(" * two").expect("the comment's second line") as u32;
        let comment: Vec<u32> = fixture
            .tokens_in(second..second + 1)
            .iter()
            .filter(|t| t.kind == SemanticTokenKind::Comment)
            .map(|t| t.line)
            .collect();
        assert_eq!(comment, vec![2]);
        assert!(fixture.tokens_in(30..30).is_empty(), "an empty range");
        assert_eq!(fixture.tokens_in(0..u32::MAX), full);
    }
}
