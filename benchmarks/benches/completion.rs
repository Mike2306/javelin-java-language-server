//! Completion latency in the two file sizes that can hurt.
//!
//! Budget: p95 under 50 ms. Two numbers, because they answer different
//! questions. `warm` is the request on its own, against a syntax view and a
//! scope tree the server already has — what an editor pays when the popup is
//! re-filtered without the buffer changing. `after_edit` is the request one
//! keystroke later, so it pays for the reparse and for rebuilding both views
//! as well; that is what typing a name actually costs.
//!
//! The gap between the two is not completion's: the server rebuilds the whole
//! syntax view and the whole scope tree for every document version, which at
//! `Tier::Huge` is around 300 ms. That work now runs on the parse pool, so it
//! no longer blocks the request loop — but it is still work, and `after_edit`
//! waits for it deliberately so the number keeps meaning "what a keystroke
//! costs" rather than "how fast the server answers from the view it has not
//! replaced yet". What the loop does meanwhile is `loop_latency`. Both numbers
//! are reported so a regression in the query itself cannot hide behind them.
//!
//! The cursor sits at the start of a statement halfway down the file, with
//! nothing typed. That is the worst case: no prefix rules anything out, so the
//! candidate cap is what bounds the answer rather than the filter.
//!
//! `member_warm` and `member_after_edit` are the same two questions asked of
//! completion after a `.` instead: the receiver is `Receiver`, a small type the
//! fixture declares purely so the dot has real members to answer with, dropped
//! in at the same halfway-down-the-file statement with a trailing dot and
//! nothing after it — the shape a real editor buffer has mid-edit. That case
//! measures finding the receiver's own declaration, which is a scan over the
//! enclosing type's scopes and declarations and grows with the tier the same
//! way `r`'s halfway-down-the-file position does for the unqualified case.
//!
//! `member_god_warm` is the case a two-member receiver deliberately does not
//! reach: the receiver is `Generated` itself, so the member walk visits the
//! tier's own 2 x N fields and methods. `this.` in a god class and a field
//! typed as one of the file's own large types are the ordinary shapes that
//! cost, and the claim that the walk is bounded by the receiver's type rather
//! than by the file is only worth making if something measures it at the size
//! where the difference shows.
//!
//! `member_external_warm` is the fourth shape, new in Phase 3b's
//! external-members slice: the receiver is a type no readable file declares,
//! so the members come out of an [`Externals`] rather than off a `Decl`. It is
//! the only case here that does **not** go through
//! `javelin_server::handlers::completion` — `ServerState`'s classpath is
//! installed by the background job and nothing public injects one, and a bench
//! that read a real JDK would cost eleven seconds and vary by machine, which
//! is exactly what a benchmark must not depend on. So this one calls
//! `javelin_semantic::member_completions_at` directly against a bench-local
//! world, the same way `overload_elimination.rs` does and for the same reason.
//! Two consequences, both deliberate: it pays no JSON serialisation and no
//! handler, so **its number is not comparable with `member_warm`'s** — and its
//! member count is fixed rather than tiered, so what moves between `large` and
//! `huge` is the file scan that finds the receiver's declaration and not the
//! member work.
//!
//! What no case here reaches: the generator types every member `int`, so
//! resolving one is a keyword lookup, and the synthetic external type types
//! every member `int` too. A member typed as a class the file declares costs a
//! `FileSyntax::declaration_at` walk of the whole file instead, which is why
//! `javelin_semantic::member_type` resolves on demand; the breadth of that is
//! pinned by a unit test in `crates/semantic`, not here. Nor does anything
//! here measure the *world's* own supertype closure walk:
//! `ClasspathExternals::members` is what pays that, and the bench-local world
//! below hands back a list it built once.

use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use javelin_benchmarks::{Tier, java_source, percentiles};
use javelin_core::{CancellationToken, Config};
use javelin_document::{Change, DocumentId, Position, Range, TextDocument};
use javelin_parser::{DeclKind, FileSyntax, JavaParser, ScopeTree, SyntaxTree};
use javelin_semantic::{
    ExternalMember, ExternalMembers, ExternalType, Externals, Files, Primitive,
};
use javelin_server::ServerState;
use lsp_types::CompletionParams;

const URI: &str = "bench://Generated.java";

/// The tiers worth measuring: a god-class and a generated one. Small and medium
/// files are bounded by these and would only add noise to the report.
const TIERS: [Tier; 2] = [Tier::Large, Tier::Huge];

fn opened(tier: Tier) -> (ServerState, DocumentId, Position) {
    let source = java_source(tier);
    let document = TextDocument::new(DocumentId::new(URI), 1, source.clone());
    let at = cursor(&document);
    let id = DocumentId::new(URI);
    let mut state = ServerState::new(Config::default()).expect("engine");
    state.did_open(id.clone(), 1, source);
    (state, id, at)
}

/// The start of a statement in a method body, halfway down the file.
fn cursor(document: &TextDocument) -> Position {
    let text = document.text();
    let middle = text.len() / 2;
    let offset = text
        .get(middle..)
        .and_then(|rest| rest.find("int local = "))
        .map_or(middle, |found| middle + found);
    document.position_at(offset as u32)
}

/// A type this fixture declares purely so a member selection has real members
/// to answer with, without the receiver's own size standing in for the walk's.
/// `member_god_warm` is the other half of that question: it uses `Generated`.
const RECEIVER_TYPE: &str = "class Receiver { int value; String label() { return null; } }\n\n";

/// The unqualified fixture, with a local of `receiver`'s type dropped in at the
/// same halfway-down-the-file statement the unqualified case uses. `Receiver`
/// is declared ahead of its one use; `Generated` is the fixture's own class and
/// needs no declaring. The dot has nothing after it, mirroring the dangling-dot
/// fixture `tests/integration/member_resolution.rs` uses for the same reason:
/// that is the shape tree-sitter's error recovery gives a real editor buffer
/// mid-edit.
fn member_source(tier: Tier, receiver: &str) -> String {
    let mut source = java_source(tier);
    if receiver == "Receiver" {
        let class_at = source
            .find("public class Generated")
            .expect("the generator's own shape");
        source.insert_str(class_at, RECEIVER_TYPE);
    }

    let marker = "int local = ";
    let middle = source.len() / 2;
    let at = source
        .get(middle..)
        .and_then(|rest| rest.find(marker))
        .map_or(middle, |found| middle + found);
    source.insert_str(at, &format!("{receiver} r; r.;\n        "));
    source
}

fn member_opened(tier: Tier, receiver: &str) -> (ServerState, DocumentId, Position) {
    let source = member_source(tier, receiver);
    let document = TextDocument::new(DocumentId::new(URI), 1, source.clone());
    let dot = document
        .text()
        .find("r.;")
        .expect("the receiver snippet was inserted")
        + "r.".len();
    let at = document.position_at(dot as u32);
    let id = DocumentId::new(URI);
    let mut state = ServerState::new(Config::default()).expect("engine");
    state.did_open(id.clone(), 1, source);
    (state, id, at)
}

/// The one external type this bench's world holds, written in the fixture as
/// a local's type. Not declared anywhere in the file, so `resolve_written`
/// falls through the file's own tiers and asks the world.
const EXTERNAL: &str = "Ext";
const EXTERNAL_FQN: &str = "com.example.ext.Ext";

/// How many members that type has. Fixed rather than tiered, and in the same
/// order as `Tier::Large`'s own 4 000 (2 000 fields and 2 000 methods): the
/// question this case answers is what a member list that size costs when it
/// arrives as facts instead of as declarations, and holding it still across
/// both tiers is what makes the `large`-to-`huge` difference readable as the
/// file scan alone.
const EXTERNAL_MEMBERS: usize = 4_096;

/// A world holding one synthetic type with [`EXTERNAL_MEMBERS`] members,
/// standing in for a classpath so the external path is measured without one.
/// `javelin-server`'s `ClasspathExternals` is not it, on purpose: this crate
/// depends on neither `javelin-classpath` nor the server's world, so however
/// long a real JDK read takes it can never regress a benchmark.
///
/// The list is built once and cloned per call, because [`Externals::members`]
/// answers by value. That clone is on the clock and it is the right thing to
/// have there: the real implementation walks a supertype closure and builds a
/// fresh vector per call too, so a world that handed out a borrow would
/// measure a request no editor makes.
#[derive(Debug)]
struct HandBuiltMembers {
    members: Vec<std::sync::Arc<ExternalMember>>,
}

impl HandBuiltMembers {
    fn new() -> Self {
        let members: Vec<ExternalMember> = (0..EXTERNAL_MEMBERS)
            .map(|i| {
                let field = i % 2 == 0;
                ExternalMember {
                    type_parameters: Vec::new(),
                    parameter_names: Vec::new(),
                    name: if field {
                        format!("field{i}")
                    } else {
                        format!("method{i}")
                    },
                    kind: if field {
                        DeclKind::Field
                    } else {
                        DeclKind::Method
                    },
                    is_static: false,
                    is_abstract: false,
                    is_public: true,
                    certain: false,
                    declared: ExternalType::Primitive(Primitive::Int),
                    parameters: if field {
                        Vec::new()
                    } else {
                        vec![ExternalType::Primitive(Primitive::Int)]
                    },
                    variadic: false,
                    owner: EXTERNAL_FQN.to_owned(),
                    ordinal: 0,
                    // A synthetic world, so there is nowhere for a site to
                    // point: `None` is the honest value, and no benchmark
                    // reads it yet.
                    site: None,
                }
            })
            .collect();
        Self {
            members: members.into_iter().map(std::sync::Arc::new).collect(),
        }
    }
}

impl Externals for HandBuiltMembers {
    fn resolve(
        &self,
        name: &str,
        _package: Option<&str>,
        _imports: &[javelin_parser::ImportDecl],
    ) -> Option<String> {
        (name == EXTERNAL).then(|| EXTERNAL_FQN.to_owned())
    }

    /// Reflexive on the one name this world holds, plus the `java.lang.Object`
    /// edge the trait requires of every reference type it knows about.
    fn is_subtype(&self, from: &str, to: &str) -> Option<bool> {
        (from == EXTERNAL_FQN).then(|| to == EXTERNAL_FQN || to == "java.lang.Object")
    }

    /// No box classes here: this world holds one type and it is not one of the
    /// eight, so `boxed` and `unboxed` answering nothing keeps the totality
    /// obligation on [`Externals::unboxed`] trivially satisfied.
    fn boxed(&self, _primitive: Primitive) -> Option<String> {
        None
    }

    fn unboxed(&self, _fqn: &str) -> Option<Primitive> {
        None
    }

    /// Complete, because a synthetic type with no supertype above it has no
    /// gap to report — which is what makes this the case where completion's
    /// `isIncomplete` can reach `false`.
    fn members(&self, fqn: &str) -> Option<ExternalMembers> {
        (fqn == EXTERNAL_FQN).then(|| ExternalMembers {
            members: self.members.clone(),
            type_parameters: Vec::new(),
            complete: true,
        })
    }
}

/// Everything `member_completions_at` needs, held rather than rebuilt per
/// iteration: the views are what the server would already have.
struct ExternalFixture {
    id: DocumentId,
    document: TextDocument,
    tree: SyntaxTree,
    syntax: FileSyntax,
    scopes: ScopeTree,
    offset: u32,
}

fn external_fixture(tier: Tier) -> ExternalFixture {
    let source = member_source(tier, EXTERNAL);
    let id = DocumentId::new(URI);
    let document = TextDocument::new(id.clone(), 1, source);
    let mut parser = JavaParser::new().expect("grammar");
    let tree = parser.parse(&document).expect("the fixture parses");
    let syntax = FileSyntax::build(&document, &tree);
    let scopes = ScopeTree::build(&document, &tree);
    let offset = (document
        .text()
        .find("r.;")
        .expect("the receiver snippet was inserted")
        + "r.".len()) as u32;
    ExternalFixture {
        id,
        document,
        tree,
        syntax,
        scopes,
        offset,
    }
}

fn complete_external(fixture: &ExternalFixture, externals: &dyn Externals) -> usize {
    let files = Files::single(fixture.id.clone(), &fixture.syntax, &fixture.scopes);
    javelin_semantic::member_completions_at(
        &fixture.document,
        &fixture.tree,
        &files,
        externals,
        fixture.offset,
    )
    .map_or(0, |offered| offered.candidates.len())
}

fn params(at: Position) -> CompletionParams {
    serde_json::from_value(serde_json::json!({
        "textDocument": { "uri": URI },
        "position": { "line": at.line, "character": at.character },
    }))
    .expect("well-formed params")
}

/// One request, including turning the answer into JSON — the client waits for
/// that too, and a completion list is the largest payload Javelin sends.
fn complete(state: &mut ServerState, params: &CompletionParams) {
    let answer =
        javelin_server::handlers::completion::handle(state, params, &CancellationToken::new())
            .expect("not cancelled");
    let _ = serde_json::to_value(answer).expect("serialisable");
}

/// A one-character edit at the cursor, which is what invalidates the tree, the
/// syntax view and the scope tree before the next request.
///
/// A replacement rather than an insertion so the buffer stays the size the tier
/// says it is however many times the loop runs.
///
/// Settled rather than left in flight: the reparse now runs on a worker, and
/// the number this bench exists for is what a keystroke costs, not how quickly
/// the server can answer from the view it has not replaced yet. The loop's own
/// responsiveness while that work happens is `loop_latency`.
fn keystroke(state: &mut ServerState, id: &DocumentId, at: Position, version: i32) {
    let next = Position::new(at.line, at.character + 1);
    state.did_change(
        id,
        version,
        &[Change::incremental(Range::new(at, next), "x")],
    );
    state.settle();
}

fn completion(c: &mut Criterion) {
    let mut group = c.benchmark_group("completion");
    for tier in TIERS {
        let (mut state, id, at) = opened(tier);
        let (mut member_state, member_id, member_at) = member_opened(tier, "Receiver");
        let (mut god_state, _, god_at) = member_opened(tier, "Generated");
        let member_params = params(member_at);
        let god_params = params(god_at);
        let params = params(at);
        complete(&mut state, &params);
        group.bench_function(BenchmarkId::new("warm", tier.name()), |b| {
            b.iter(|| complete(&mut state, &params));
        });

        let mut version = 1;
        group.bench_function(BenchmarkId::new("after_edit", tier.name()), |b| {
            b.iter(|| {
                version += 1;
                keystroke(&mut state, &id, at, version);
                complete(&mut state, &params);
            });
        });

        complete(&mut member_state, &member_params);
        group.bench_function(BenchmarkId::new("member_warm", tier.name()), |b| {
            b.iter(|| complete(&mut member_state, &member_params));
        });

        let mut member_version = 1;
        group.bench_function(BenchmarkId::new("member_after_edit", tier.name()), |b| {
            b.iter(|| {
                member_version += 1;
                keystroke(&mut member_state, &member_id, member_at, member_version);
                complete(&mut member_state, &member_params);
            });
        });

        // Warm only. The `after_edit` counterpart would re-measure the
        // whole-document view rebuild that `after_edit` and
        // `member_after_edit` already report, and the question this case
        // exists for is the member walk on its own.
        complete(&mut god_state, &god_params);
        group.bench_function(BenchmarkId::new("member_god_warm", tier.name()), |b| {
            b.iter(|| complete(&mut god_state, &god_params));
        });

        // Asserted once, outside the timed loop: a benchmark of an empty list
        // would be fast and meaningless, and the cap is what the answer is
        // bounded by rather than the 4 096 members behind it.
        let external = external_fixture(tier);
        let world = HandBuiltMembers::new();
        assert_eq!(complete_external(&external, &world), 500, "{:?}", tier);
        group.bench_function(BenchmarkId::new("member_external_warm", tier.name()), |b| {
            b.iter(|| complete_external(&external, &world));
        });
    }
    group.finish();

    // `cargo bench` passes `--bench` to the harness and `cargo test` passes
    // nothing, which is how Criterion itself tells the two apart. Under
    // `--all-targets` it short-circuits its own closures, but the loops below
    // are plain code: unguarded and unoptimised they put three minutes into
    // every CI run on every platform, for numbers nobody reads there.
    if !std::env::args().any(|argument| argument == "--bench") {
        return;
    }

    // Criterion reports central tendency; the budget is stated in percentiles.
    for tier in TIERS {
        let (mut state, id, at) = opened(tier);
        let (mut member_state, member_id, member_at) = member_opened(tier, "Receiver");
        let (mut god_state, _, god_at) = member_opened(tier, "Generated");
        let member_params = params(member_at);
        let god_params = params(god_at);
        let params = params(at);
        complete(&mut state, &params);

        let mut warm: Vec<Duration> = (0..200)
            .map(|_| {
                let started = Instant::now();
                complete(&mut state, &params);
                started.elapsed()
            })
            .collect();
        report(tier, "warm", &mut warm);

        let mut edited: Vec<Duration> = (0..200)
            .map(|version| {
                let started = Instant::now();
                keystroke(&mut state, &id, at, version + 2);
                complete(&mut state, &params);
                started.elapsed()
            })
            .collect();
        report(tier, "after_edit", &mut edited);

        complete(&mut member_state, &member_params);

        let mut member_warm: Vec<Duration> = (0..200)
            .map(|_| {
                let started = Instant::now();
                complete(&mut member_state, &member_params);
                started.elapsed()
            })
            .collect();
        report(tier, "member_warm", &mut member_warm);

        let mut member_edited: Vec<Duration> = (0..200)
            .map(|version| {
                let started = Instant::now();
                keystroke(&mut member_state, &member_id, member_at, version + 2);
                complete(&mut member_state, &member_params);
                started.elapsed()
            })
            .collect();
        report(tier, "member_after_edit", &mut member_edited);

        complete(&mut god_state, &god_params);

        let mut god_warm: Vec<Duration> = (0..200)
            .map(|_| {
                let started = Instant::now();
                complete(&mut god_state, &god_params);
                started.elapsed()
            })
            .collect();
        report(tier, "member_god_warm", &mut god_warm);

        let external = external_fixture(tier);
        let world = HandBuiltMembers::new();
        let mut external_warm: Vec<Duration> = (0..200)
            .map(|_| {
                let started = Instant::now();
                complete_external(&external, &world);
                started.elapsed()
            })
            .collect();
        report(tier, "member_external_warm", &mut external_warm);
    }
}

fn report(tier: Tier, what: &str, samples: &mut [Duration]) {
    if let Some(stats) = percentiles(samples) {
        println!(
            "completion {what} {}: p50={:?} p95={:?} p99={:?} max={:?} (budget: p95 < 50ms)",
            tier.name(),
            stats.p50,
            stats.p95,
            stats.p99,
            stats.max
        );
    }
}

criterion_group!(benches, completion);
criterion_main!(benches);
