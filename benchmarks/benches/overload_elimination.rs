//! The argument-type elimination path: `sole_candidate` actually running
//! `apply.rs`'s exclusion over every candidate, rather than exiting on the
//! first name match.
//!
//! Never benchmarked before Task 9, and not covered by
//! `a_receiver_with_thousands_of_members_answers_without_walking_the_file_per_member`
//! in `crates/semantic/src/queries.rs` either: that test goes through
//! `member_completions_at`, which lists members and never calls
//! `sole_candidate` at all. This bench goes through `member_definition_at`, the
//! query `sole_candidate` actually sits behind, on `this.over(1)` against the
//! `over(int)` / `over(Generated)` / `over(String)` set `java_source` now adds
//! once per tier.
//!
//! `over(Generated)` is excluded immediately — `int` against a file-local
//! declared type is `apply.rs`'s cheapest certainty. What is left standing
//! depends on what the world can say about `over(String)`'s parameter, which
//! is why this bench runs it two ways:
//!
//! - `no_externals`: the default every other benchmark in this crate runs
//!   under. `String` does not resolve, so `over(String)` is not excluded
//!   either, two candidates survive, and `sole_candidate` refuses — the
//!   ambiguity in `definition_on_two_overloads_differing_only_in_a_jdk_
//!   parameter_type_answers_nothing` in `crates/semantic/src/queries.rs`. That
//!   refusal is not a fast path: every candidate still had to be checked
//!   before elimination could tell they were tied.
//! - `hand_built_classpath`: a bench-local `Externals` over three fixed facts
//!   (`java.lang.String`, `java.lang.Integer`, `java.lang.Object`), never a
//!   real JDK read — that costs eleven seconds and varies by machine, which is
//!   exactly what a benchmark must not depend on. `String` resolves, boxing
//!   `int` to `java.lang.Integer` and asking whether that is a `String`
//!   answers `Some(false)`, so `over(String)` is excluded too and `over(int)`
//!   is the sole survivor. Still a full walk over every candidate — elimination
//!   does not stop early once it has one exclusion, because there is no way to
//!   tell that the *next* candidate would not also need one — so this variant
//!   answers rather than refuses, but costs the same shape of work.
//!
//! Both variants walk `members_of(Generated)` twice, once per namespace, over
//! the tier's own thousands of fields and methods before either exclusion
//! runs at all — that walk, not the exclusion arithmetic, is what should
//! dominate at `Tier::Huge`.

use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use javelin_benchmarks::{Tier, java_source, percentiles};
use javelin_document::{DocumentId, TextDocument};
use javelin_parser::{FileSyntax, JavaParser, ScopeTree, SyntaxTree};
use javelin_semantic::{ExternalMembers, Externals, NoExternals, Primitive};

/// Three JDK facts, fixed and deterministic, standing in for a classpath read
/// so the classpath-backed path is measured without one. `javelin-server`'s
/// `ClasspathExternals` is not it: this crate does not depend on
/// `javelin-classpath` or `javelin-server`'s world, on purpose, so this
/// benchmark cannot regress by however long a real JDK read takes.
#[derive(Debug)]
struct HandBuiltClasspath;

const KNOWN: [&str; 3] = ["java.lang.String", "java.lang.Integer", "java.lang.Object"];

impl Externals for HandBuiltClasspath {
    /// The three names this world holds, unqualified as `java_source` writes
    /// them — no import, no package qualification to resolve against, because
    /// none of the three is ever written any other way in the fixture.
    fn resolve(
        &self,
        name: &str,
        _package: Option<&str>,
        _imports: &[javelin_parser::ImportDecl],
    ) -> Option<String> {
        match name {
            "String" => Some("java.lang.String".to_owned()),
            "Integer" => Some("java.lang.Integer".to_owned()),
            "Object" => Some("java.lang.Object".to_owned()),
            _ => None,
        }
    }

    /// Reflexive on the three names this world holds, per the trait's
    /// obligation, plus the one non-reflexive fact `apply.rs` needs to
    /// exclude `over(String)`: an `Integer` is not a `String`. `Integer` and
    /// `String` are each `Object`'s subtype for the same reason a real JDK
    /// would answer so, though nothing here asks for it.
    fn is_subtype(&self, from: &str, to: &str) -> Option<bool> {
        if !KNOWN.contains(&from) || !KNOWN.contains(&to) {
            return None;
        }
        Some(from == to || (from != "java.lang.Object" && to == "java.lang.Object"))
    }

    fn boxed(&self, primitive: Primitive) -> Option<String> {
        (primitive == Primitive::Int).then(|| "java.lang.Integer".to_owned())
    }

    /// Total over the box classes this world holds, per the trait's
    /// obligation: the one box class in `KNOWN` is `java.lang.Integer`, and it
    /// answers here whenever [`Self::boxed`] would produce it.
    fn unboxed(&self, fqn: &str) -> Option<Primitive> {
        (fqn == "java.lang.Integer").then_some(Primitive::Int)
    }

    /// No members, so an external receiver offers nothing here. What this
    /// benchmark measures is the walk over the *generated* type's thousands of
    /// members and the elimination that follows it, and answering members for
    /// `java.lang.String` would change what is on the clock rather than add to
    /// it.
    fn members(&self, _fqn: &str) -> Option<ExternalMembers> {
        None
    }
}

const URI: &str = "bench://Generated.java";

struct Fixture {
    /// Held rather than rebuilt per iteration: the server clones an id it
    /// already has, and allocating one inside the measured function would
    /// charge this benchmark for work no request does.
    id: DocumentId,
    document: TextDocument,
    tree: SyntaxTree,
    syntax: FileSyntax,
    scopes: ScopeTree,
    offset: u32,
}

fn fixture(tier: Tier) -> Fixture {
    let source = java_source(tier);
    let id = DocumentId::new(URI);
    let document = TextDocument::new(id.clone(), 1, source);
    let mut parser = JavaParser::new().expect("grammar");
    let tree = parser.parse(&document).expect("the fixture parses");
    let syntax = FileSyntax::build(&document, &tree);
    let scopes = ScopeTree::build(&document, &tree);
    let text = document.text();
    let marker = "this.over(1)";
    let at = text.find(marker).expect("java_source adds this call site");
    let offset = (at + "this.".len()) as u32;
    Fixture {
        id,
        document,
        tree,
        syntax,
        scopes,
        offset,
    }
}

fn resolve(fixture: &Fixture, externals: &dyn Externals) -> Option<javelin_parser::Definition> {
    let files =
        javelin_semantic::Files::single(fixture.id.clone(), &fixture.syntax, &fixture.scopes);
    javelin_semantic::member_definition_at(
        &fixture.document,
        &fixture.tree,
        &files,
        externals,
        fixture.offset,
    )
}

fn overload_elimination(c: &mut Criterion) {
    let mut group = c.benchmark_group("overload_elimination");
    for tier in Tier::ALL {
        let fixture = fixture(tier);

        // `no_externals` refuses — two candidates survive, per the module
        // doc — and `hand_built_classpath` answers `over(int)`. Both are
        // asserted once, outside the timed loop, so a regression that changed
        // *which* answer came back would fail loudly rather than silently
        // benchmark the wrong path.
        assert!(resolve(&fixture, &NoExternals).is_none(), "{:?}", tier);
        assert_eq!(
            resolve(&fixture, &HandBuiltClasspath)
                .expect("over(int), the sole survivor")
                .signature,
            "void over(int i)",
            "{:?}",
            tier
        );

        group.bench_function(BenchmarkId::new("no_externals", tier.name()), |b| {
            b.iter(|| resolve(&fixture, &NoExternals));
        });
        group.bench_function(BenchmarkId::new("hand_built_classpath", tier.name()), |b| {
            b.iter(|| resolve(&fixture, &HandBuiltClasspath));
        });
    }
    group.finish();

    // Same rule every other bench in this crate follows: `cargo bench` passes
    // `--bench`, `cargo test` does not, and the loops below would otherwise
    // run unbounded on every CI job.
    if !std::env::args().any(|argument| argument == "--bench") {
        return;
    }

    for tier in Tier::ALL {
        let fixture = fixture(tier);

        let mut no_externals: Vec<Duration> = (0..200)
            .map(|_| {
                let started = Instant::now();
                resolve(&fixture, &NoExternals);
                started.elapsed()
            })
            .collect();
        report(tier, "no_externals", &mut no_externals);

        let mut hand_built: Vec<Duration> = (0..200)
            .map(|_| {
                let started = Instant::now();
                resolve(&fixture, &HandBuiltClasspath);
                started.elapsed()
            })
            .collect();
        report(tier, "hand_built_classpath", &mut hand_built);
    }
}

fn report(tier: Tier, what: &str, samples: &mut [Duration]) {
    if let Some(stats) = percentiles(samples) {
        println!(
            "overload_elimination {what} {}: p50={:?} p95={:?} p99={:?} max={:?} \
             (budget: p95 < 50ms, same as definition)",
            tier.name(),
            stats.p50,
            stats.p95,
            stats.p99,
            stats.max
        );
    }
}

criterion_group!(benches, overload_elimination);
criterion_main!(benches);
