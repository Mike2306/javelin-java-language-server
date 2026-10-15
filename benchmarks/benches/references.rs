//! `textDocument/references` latency, against the 100 ms local-references
//! budget.
//!
//! Two groups, because the two halves of the answer cost different things.
//!
//! `references` goes through the handler on the generated fixture, at every
//! tier, with the cursor on the middle method's `local`. That is the in-file
//! half at its worst: every generated method declares its own `local`, so the
//! file holds four tokens spelled `local` per method and every one of them is
//! resolved the way go-to-definition would resolve it, to keep the four that
//! land on the one under the cursor. `field` and `call` are the other two
//! shapes: a field with one use, and a method with one call site.
//!
//! `references_workspace` is the cross-file half, and does not go through the
//! handler for the reason `definition.rs`'s external rows do not: the
//! workspace index is installed by the background walk and nothing public
//! injects one. What runs below is the loop the handler runs over the index —
//! every occurrence spelled like the target, each resolved through
//! `javelin_classpath::simple_name` against its own file's package and
//! imports — over a generated index of [`FILES`] files that each name the
//! target [`PER_FILE`] times. It pays no JDK half of the table and no JSON.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use javelin_benchmarks::{Tier, java_source, percentiles};
use javelin_classpath::{Resolution, TypeTable, simple_name};
use javelin_core::{CancellationToken, Config};
use javelin_document::{DocumentId, Position, Range, TextDocument};
use javelin_index::{WorkspaceFile, WorkspaceIndex, WorkspaceOccurrence, WorkspaceType};
use javelin_parser::{DeclKind, ImportDecl, Span};
use javelin_server::ServerState;
use lsp_types::{Location, ReferenceParams};

const URI: &str = "bench://Generated.java";

/// Files in the generated workspace index, and how often each names the
/// target: twenty thousand rows spelled like it.
const FILES: usize = 2_000;
const PER_FILE: usize = 10;

fn opened(tier: Tier) -> ServerState {
    let mut state = ServerState::new(Config::default()).expect("engine");
    state.did_open(DocumentId::new(URI), 1, java_source(tier));
    state
}

/// The start of the first `marker` at or after the middle of the file, plus
/// `skip` bytes — `definition.rs`'s convention, so positions grow with the
/// tier the same way.
fn position_after(document: &TextDocument, marker: &str, skip: usize) -> Position {
    let text = document.text();
    let middle = text.len() / 2;
    let found = text
        .get(middle..)
        .and_then(|rest| rest.find(marker))
        .map_or(middle, |found| middle + found);
    document.position_at((found + skip) as u32)
}

fn params(at: Position) -> ReferenceParams {
    serde_json::from_value(serde_json::json!({
        "textDocument": { "uri": URI },
        "position": { "line": at.line, "character": at.character },
        "context": { "includeDeclaration": true },
    }))
    .expect("well-formed params")
}

/// One request, JSON included.
fn references(state: &mut ServerState, params: &ReferenceParams) -> Vec<Location> {
    let answer =
        javelin_server::handlers::references::handle(state, params, &CancellationToken::new())
            .expect("not cancelled");
    let _ = serde_json::to_value(&answer).expect("serialisable");
    answer
}

fn in_file(c: &mut Criterion) {
    let mut group = c.benchmark_group("references");
    for tier in Tier::ALL {
        let document = TextDocument::new(DocumentId::new(URI), 1, java_source(tier));
        let cases = [
            // `int local = …`: the declaration and its three uses.
            (
                "local",
                position_after(&document, "int local = ", "int ".len()),
                4,
            ),
            // `private int field{i} = …`: the declaration and one use.
            (
                "field",
                position_after(&document, "private int field", "private int ".len()),
                2,
            ),
            // `public int method{i}(…`: the declaration and one call site.
            (
                "call",
                position_after(&document, "public int method", "public int ".len()),
                2,
            ),
        ];
        for (name, at, expected) in cases {
            let mut state = opened(tier);
            let request = params(at);
            let found = references(&mut state, &request).len();
            assert_eq!(
                found,
                expected,
                "{name} at {} must find its declaration and uses",
                tier.name()
            );
            group.bench_function(BenchmarkId::new(name, tier.name()), |b| {
                b.iter(|| references(&mut state, &request));
            });

            if std::env::args().any(|argument| argument == "--bench") {
                let mut samples: Vec<Duration> = (0..50)
                    .map(|_| {
                        let started = Instant::now();
                        references(&mut state, &request);
                        started.elapsed()
                    })
                    .collect();
                if let Some(stats) = percentiles(&mut samples) {
                    println!(
                        "references {name} {}: p50={:?} p95={:?} max={:?} (budget: p95 < 100ms)",
                        tier.name(),
                        stats.p50,
                        stats.p95,
                        stats.max
                    );
                }
            }
        }
    }
    group.finish();
}

fn zero() -> Span {
    Span {
        start_byte: 0,
        end_byte: 0,
        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
    }
}

/// `FILES` files in twenty packages. Half import the target by name, the
/// other half live in its package, so every row resolves — the most work a
/// row can cost.
fn workspace() -> WorkspaceIndex {
    let target = WorkspaceType {
        is_public: true,
        doc_span: None,
        fqn: "com.example.Target".to_owned(),
        simple: "Target".to_owned(),
        kind: DeclKind::Class,
        file: PathBuf::from("/w/com/example/Target.java"),
        name_start: 0,
        name_end: 0,
        decl_start: 0,
        decl_end: 0,
        name_range: Range::new(Position::new(0, 13), Position::new(0, 19)),
        supertypes: Vec::new(),
        synthetic: false,
        lombok: Default::default(),
        type_parameters: Vec::new(),
    };
    let mut sources = Vec::new();
    let mut occurrences = Vec::new();
    for file in 0..FILES {
        let imports = file % 2 == 0;
        sources.push(WorkspaceFile {
            path: PathBuf::from(format!("/w/p{}/F{file}.java", file % 20)),
            package: Some(if imports {
                format!("p{}", file % 20)
            } else {
                "com.example".to_owned()
            }),
            imports: if imports {
                vec![ImportDecl {
                    name: "com.example.Target".to_owned(),
                    span: zero(),
                    name_span: zero(),
                    is_static: false,
                    is_on_demand: false,
                }]
            } else {
                Vec::new()
            },
            parsed_cleanly: true,
        });
        for line in 0..PER_FILE as u32 {
            occurrences.push(WorkspaceOccurrence {
                file,
                name: "Target".to_owned(),
                start_byte: 0,
                end_byte: 0,
                range: Range::new(Position::new(line, 4), Position::new(line, 10)),
                fqn: None,
            });
        }
    }
    WorkspaceIndex::with_occurrences(vec![target], sources, occurrences)
}

struct Table<'a>(&'a WorkspaceIndex);

impl TypeTable for Table<'_> {
    fn contains(&self, fqn: &str) -> bool {
        self.0.with_fqn(fqn).next().is_some()
    }
}

/// The handler's loop over the index, and how many rows it kept.
fn across(index: &WorkspaceIndex, fqn: &str) -> usize {
    let table = Table(index);
    index
        .occurrences_named(fqn.rsplit('.').next().unwrap_or(fqn))
        .filter(|occurrence| {
            let file = index.file(occurrence.file);
            simple_name(
                &occurrence.name,
                file.package.as_deref(),
                &file.imports,
                &table,
            ) == Resolution::One(fqn.to_owned())
        })
        .count()
}

fn across_workspace(c: &mut Criterion) {
    let index = workspace();
    assert_eq!(across(&index, "com.example.Target"), FILES * PER_FILE);
    let mut group = c.benchmark_group("references_workspace");
    group.bench_function("type", |b| b.iter(|| across(&index, "com.example.Target")));
    group.finish();

    if std::env::args().any(|argument| argument == "--bench") {
        let mut samples: Vec<Duration> = (0..200)
            .map(|_| {
                let started = Instant::now();
                across(&index, "com.example.Target");
                started.elapsed()
            })
            .collect();
        if let Some(stats) = percentiles(&mut samples) {
            println!(
                "references_workspace type ({} rows): p50={:?} p95={:?} max={:?} (budget: p95 < 100ms)",
                FILES * PER_FILE,
                stats.p50,
                stats.p95,
                stats.max
            );
        }
    }
}

/// The scan references on a constructor makes for every `this(…)`,
/// `super(…)` and enum constant in the open file, at every tier. A walk over
/// every node that asked each its parent was quadratic in a flat class body,
/// because tree-sitter finds a parent by searching from the root.
fn explicit_calls(c: &mut Criterion) {
    let mut group = c.benchmark_group("references_explicit_calls");
    for tier in Tier::ALL {
        let document = TextDocument::new(DocumentId::new(URI), 1, java_source(tier));
        let tree = javelin_parser::JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let token = CancellationToken::new();
        let scan = || javelin_parser::explicit_constructor_spans(&document, &tree, &token);
        assert!(scan().expect("not cancelled").is_empty());
        group.bench_function(BenchmarkId::new("scan", tier.name()), |b| b.iter(scan));
        if std::env::args().any(|argument| argument == "--bench") {
            let mut samples: Vec<Duration> = (0..20)
                .map(|_| {
                    let started = Instant::now();
                    let _ = scan();
                    started.elapsed()
                })
                .collect();
            if let Some(stats) = percentiles(&mut samples) {
                println!(
                    "references explicit_calls {}: p50={:?} p95={:?} max={:?} (budget: p95 < 100ms)",
                    tier.name(),
                    stats.p50,
                    stats.p95,
                    stats.max
                );
            }
        }
    }
    group.finish();
}

criterion_group!(benches, in_file, across_workspace, explicit_calls);
criterion_main!(benches);
