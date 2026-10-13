//! Parse throughput across file sizes.
//!
//! Budget: a typical file must parse in well under 10 ms. This is the `didOpen`
//! cost; the per-keystroke cost lives in the `documents` bench, which measures a
//! full and an incremental reparse side by side.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use javelin_benchmarks::{Tier, java_source};
use javelin_document::{DocumentId, TextDocument};
use javelin_parser::JavaParser;

fn parse_by_size(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse");
    for tier in Tier::ALL {
        let source = java_source(tier);
        let document =
            TextDocument::new(DocumentId::new("bench://Generated.java"), 1, source.clone());
        group.throughput(Throughput::Bytes(source.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(tier.name()),
            &document,
            |b, doc| {
                let mut parser = JavaParser::new().expect("grammar");
                b.iter(|| parser.parse(doc).expect("parse"));
            },
        );
    }
    group.finish();
}

/// Loading the grammar is paid once per parser. It sits on the startup path, so
/// it is measured separately from parsing itself.
fn parser_construction(c: &mut Criterion) {
    c.bench_function("parser/new", |b| {
        b.iter(|| JavaParser::new().expect("grammar"))
    });
}

criterion_group!(benches, parse_by_size, parser_construction);
criterion_main!(benches);
