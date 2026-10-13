//! Incremental reparsing must be indistinguishable from parsing from scratch.
//!
//! Tree-sitter reuses subtrees on trust: given an old tree and edits that do not
//! describe how the text actually moved, it returns a tree that is wrong rather
//! than one that fails. Nothing in the parser catches that — only a comparison
//! against a full parse of the same text does, which is what this file is.
//!
//! Trees are compared by the root S-expression *and* by every node's byte and
//! point range. The S-expression alone would miss ranges that drifted while the
//! shape stayed right, which is the exact failure a bad `InputEdit` produces.

use javelin_document::{Change, DocumentId, Position, Range, TextDocument};
use javelin_integration_tests::fixture;
use javelin_parser::{JavaParser, SyntaxTree};

/// xorshift64*, so a failing run reproduces from its seed alone. A PRNG
/// dependency would cost startup budget for something this file needs six lines
/// of arithmetic to do.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() % bound as u64) as usize
        }
    }
}

/// One open file, edited repeatedly, checked after every change.
struct Session {
    parser: JavaParser,
    document: TextDocument,
    tree: SyntaxTree,
    rng: Rng,
    name: &'static str,
    seed: u64,
}

impl Session {
    fn open(name: &'static str, text: String, seed: u64) -> Self {
        let mut parser = JavaParser::new().expect("grammar");
        let document = TextDocument::new(DocumentId::new(format!("file:///{name}")), 1, text);
        let tree = parser.parse(&document).expect("parse");
        Self {
            parser,
            document,
            tree,
            rng: Rng(seed),
            name,
            seed,
        }
    }

    /// Apply changes, reparse both ways, and require the results to agree.
    ///
    /// The incrementally produced tree is the one kept, so later edits build on
    /// a reused tree rather than a fresh one — a divergence that only shows up
    /// after several edits still shows up here.
    fn apply(&mut self, step: &str, changes: &[Change]) {
        let version = self.document.version() + 1;
        let (next, edits) = self.document.with_changes_reporting(version, changes);
        let incremental = self
            .parser
            .reparse(&self.tree, &edits, &next)
            .expect("reparse");
        let fresh = self.parser.parse(&next).expect("parse");

        let context = format!("{} seed {} step {step} v{version}", self.name, self.seed);
        assert_eq!(
            incremental.root().to_sexp(),
            fresh.root().to_sexp(),
            "tree shape diverged: {context}"
        );
        assert_eq!(
            ranges(&incremental),
            ranges(&fresh),
            "node ranges diverged: {context}"
        );
        assert_eq!(
            incremental.has_errors(),
            fresh.has_errors(),
            "error recovery diverged: {context}"
        );

        self.document = next;
        self.tree = incremental;
    }

    fn insert(&mut self, step: &str, offset: usize, text: &str) {
        let at = self.position(offset);
        self.apply(step, &[Change::incremental(Range::new(at, at), text)]);
    }

    fn replace(&mut self, step: &str, offset: usize, len: usize, text: &str) {
        let start = self.position(offset);
        let end = self.position(offset + len);
        self.apply(step, &[Change::incremental(Range::new(start, end), text)]);
    }

    fn position(&self, offset: usize) -> Position {
        self.document
            .position_at(boundary(self.document.text(), offset) as u32)
    }

    fn random_offset(&mut self) -> usize {
        let len = self.document.text().len();
        self.rng.below(len + 1)
    }
}

/// Every node's kind and both its byte and point range, in traversal order.
fn ranges(tree: &SyntaxTree) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![tree.root()];
    while let Some(node) = stack.pop() {
        let (start, end) = (node.start_position(), node.end_position());
        out.push(format!(
            "{} {}..{} {}:{}..{}:{}",
            node.kind(),
            node.start_byte(),
            node.end_byte(),
            start.row,
            start.column,
            end.row,
            end.column
        ));
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    out
}

/// Snap an offset down to a character boundary, so a generated edit can aim at
/// the middle of an emoji without the test itself panicking.
fn boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// Text fragments chosen to disturb the parse as much as possible: they open and
/// close constructs, span lines, and are not all ASCII.
const PALETTE: [&str; 10] = [
    "x",
    "\n",
    "}",
    "{",
    "\"",
    "*/",
    "é",
    "🚀",
    " int y = 1;\n",
    ";",
];

fn hammer(name: &'static str, source: String, seed: u64, replacement: String) {
    let mut session = Session::open(name, source, seed);

    // Give every fixture a line comment and a string literal to edit inside,
    // and cover the two offsets most likely to be handled specially.
    session.insert("insert at offset 0", 0, "// injected\n");
    let end = session.document.text().len();
    session.insert(
        "insert at end of file",
        end,
        "\nclass Tail { String s = \"seed\"; }\n",
    );

    let in_comment = session.document.text().find("//").expect("line comment") + 2;
    session.insert("inside a comment", in_comment, " edited");

    let in_string = session
        .document
        .text()
        .find("\"seed\"")
        .expect("string literal")
        + 3;
    session.insert("inside a string literal", in_string, "🚀 ee");

    // At a multi-byte boundary: immediately before and immediately after the
    // first character that is not one byte wide.
    let multibyte = session
        .document
        .text()
        .char_indices()
        .find(|(_, c)| c.len_utf8() > 1)
        .map(|(offset, c)| (offset, c.len_utf8()))
        .expect("the fixtures all carry a multi-byte character by now");
    session.insert("before a multi-byte character", multibyte.0, "ä");
    session.insert(
        "after a multi-byte character",
        multibyte.0 + 2 + multibyte.1,
        "ö",
    );

    session.replace("delete a span", 20, 12, "");
    session.replace("replace a span", 12, 6, "renamed");

    // A batch: three changes in one notification, each resolved against the text
    // its predecessor produced.
    let first = session.position(30);
    let second = session.position(45);
    session.apply(
        "a batch of three changes",
        &[
            Change::incremental(Range::new(first, first), "/* one */"),
            Change::incremental(Range::new(second, second), "\nint two;\n"),
            Change::incremental(Range::new(first, second), "three"),
        ],
    );

    for round in 0..24 {
        let step = format!("random round {round}");
        let offset = session.random_offset();
        let text = PALETTE[session.rng.below(PALETTE.len())];
        match session.rng.below(3) {
            0 => session.insert(&step, offset, text),
            1 => {
                let len = session.rng.below(24);
                session.replace(&step, offset, len, "");
            }
            _ => {
                let len = session.rng.below(12);
                session.replace(&step, offset, len, text);
            }
        }
    }

    session.apply("full replacement", &[Change::full(replacement)]);
    session.insert("edit after a full replacement", 0, "// after\n");
}

#[test]
fn reparsing_a_well_formed_file_matches_a_full_parse() {
    hammer(
        "Simple.java",
        fixture("java/Simple.java"),
        0x5eed_0001,
        fixture("java/Unicode.java"),
    );
}

#[test]
fn reparsing_a_file_the_grammar_recovers_from_matches_a_full_parse() {
    hammer(
        "Broken.java",
        fixture("java/Broken.java"),
        0x5eed_0002,
        fixture("java/Simple.java"),
    );
}

#[test]
fn reparsing_a_file_full_of_multi_byte_characters_matches_a_full_parse() {
    hammer(
        "Unicode.java",
        fixture("java/Unicode.java"),
        0x5eed_0003,
        fixture("java/Broken.java"),
    );
}

#[test]
fn a_tree_from_a_different_version_is_never_reused() {
    let mut parser = JavaParser::new().expect("grammar");
    let document = TextDocument::new(
        DocumentId::new("file:///A.java"),
        1,
        fixture("java/Simple.java"),
    );
    // Edits computed from version 1, applied to a tree that is already ahead.
    let (v2, edits) = document.with_changes_reporting(2, &[Change::full("class Other {}")]);
    let ahead = parser.parse(&v2).expect("parse");
    let (v3, _) = v2.with_changes_reporting(3, &[Change::full("class Third {}")]);

    let reparsed = parser.reparse(&ahead, &edits, &v3).expect("reparse");
    assert_eq!(
        reparsed.root().to_sexp(),
        parser.parse(&v3).expect("parse").root().to_sexp()
    );
}
