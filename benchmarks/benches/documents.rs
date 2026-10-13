//! Document store and position mapping, and the parse each keystroke pays for.
//!
//! The store side runs on every keystroke and is pure overhead — none of it
//! produces an answer the user sees, so it needs to stay far below the parse
//! cost. The parse side is measured here too, full against incremental, so the
//! saving an edit actually buys is visible in one report.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use javelin_benchmarks::{Tier, java_source};
use javelin_document::{Change, DocumentId, DocumentStore, Position, Range, TextDocument};
use javelin_parser::JavaParser;

fn open_document(c: &mut Criterion) {
    let mut group = c.benchmark_group("document/open");
    for tier in Tier::ALL {
        let source = java_source(tier);
        group.bench_with_input(
            BenchmarkId::from_parameter(tier.name()),
            &source,
            |b, text| {
                b.iter(|| {
                    let mut store = DocumentStore::new();
                    store.open(DocumentId::new("bench://Generated.java"), 1, text.clone())
                });
            },
        );
    }
    group.finish();
}

/// A single-character insertion, the shape of an actual keystroke.
fn apply_keystroke(c: &mut Criterion) {
    let mut group = c.benchmark_group("document/keystroke");
    for tier in Tier::ALL {
        let document = TextDocument::new(
            DocumentId::new("bench://Generated.java"),
            1,
            java_source(tier),
        );
        let at = Position::new(2, 0);
        let change = Change::incremental(Range::new(at, at), "x");
        group.bench_with_input(
            BenchmarkId::from_parameter(tier.name()),
            &document,
            |b, doc| {
                b.iter(|| doc.with_changes(2, std::slice::from_ref(&change)));
            },
        );
    }
    group.finish();
}

/// The same keystroke, reparsed both ways.
///
/// `full` is what the server did before it tracked edits; `incremental` is what
/// it does now, tree copy included, because the server pays for that too.
fn reparse_after_keystroke(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse/keystroke");
    for tier in Tier::ALL {
        let document = TextDocument::new(
            DocumentId::new("bench://Generated.java"),
            1,
            java_source(tier),
        );
        // Mid-file, mid-token: a user typing inside a method body.
        let at = document.position_at(document.text().len() as u32 / 2);
        let change = Change::incremental(Range::new(at, at), "x");
        let (edited, edits) = document.with_changes_reporting(2, std::slice::from_ref(&change));

        let mut parser = JavaParser::new().expect("grammar");
        let before = parser.parse(&document).expect("parse");

        group.bench_with_input(BenchmarkId::new("full", tier.name()), &edited, |b, doc| {
            b.iter(|| parser.parse(doc).expect("parse"));
        });
        group.bench_with_input(
            BenchmarkId::new("incremental", tier.name()),
            &edited,
            |b, doc| {
                b.iter(|| parser.reparse(&before, &edits, doc).expect("reparse"));
            },
        );
    }
    group.finish();
}

fn position_lookup(c: &mut Criterion) {
    let document = TextDocument::new(
        DocumentId::new("bench://Generated.java"),
        1,
        java_source(Tier::Large),
    );
    let end = document.text().len() as u32 - 1;
    c.bench_function("document/position_at_end_of_large_file", |b| {
        b.iter(|| document.position_at(end));
    });
}

criterion_group!(
    benches,
    open_document,
    apply_keystroke,
    reparse_after_keystroke,
    position_lookup
);
criterion_main!(benches);
