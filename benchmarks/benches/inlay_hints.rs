//! `textDocument/inlayHint` latency, across the generated fixture's tiers:
//! a screen of sixty lines, and the whole file a client like Neovim asks for.
//!
//! Budget: p95 under 50 ms. The handler resolves every call it hints the
//! way definition does, on the request loop, so it carries its own bound — a
//! call count and a 20 ms deadline — and the `file` row is that bound being
//! measured rather than the work a whole file would take.
//!
//! Every generated method holds a `var` read of an `int` local, so each one
//! is a type hint typed through `type_of` as well as a call resolved, and
//! the two kinds share the deadline in source order.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use javelin_benchmarks::{Tier, java_source};
use javelin_core::{CancellationToken, Config};
use javelin_document::DocumentId;
use javelin_server::ServerState;
use lsp_types::InlayHintParams;

const URI: &str = "bench://Generated.java";

fn params(lines: u32) -> InlayHintParams {
    serde_json::from_value(serde_json::json!({
        "textDocument": { "uri": URI },
        "range": { "start": { "line": 0, "character": 0 }, "end": { "line": lines, "character": 0 } },
    }))
    .expect("well-formed params")
}

/// One request, JSON included — the client waits for that too. Answers how
/// many hints of each kind, parameter and type.
fn hints(state: &mut ServerState, params: &InlayHintParams) -> (usize, usize) {
    let answer =
        javelin_server::handlers::inlay_hint::handle(state, params, &CancellationToken::new())
            .expect("not cancelled");
    let _ = serde_json::to_value(&answer).expect("serialisable");
    let types = answer
        .iter()
        .filter(|hint| hint.kind == Some(lsp_types::InlayHintKind::TYPE))
        .count();
    (answer.len() - types, types)
}

fn inlay_hints(c: &mut Criterion) {
    let mut group = c.benchmark_group("inlay_hints");
    for tier in Tier::ALL {
        // The fixture's calls pass `argument` to a parameter named
        // `argument`, which is left bare; a literal is hinted, so every call
        // is resolved *and* answers.
        let source = java_source(tier)
            .replace("(argument);", "(1);")
            .replace("var doubled = local * 2;", "var doubled = local;");
        let lines = source.lines().count() as u32;
        let mut state = ServerState::new(Config::default()).expect("engine");
        state.did_open(DocumentId::new(URI), 1, source);
        let screen = params(60);
        // The deadline may cut the first, cold request short on a slow
        // machine — an unoptimised `cargo test` on a CI runner stops the
        // huge tier after one type hint. Each request moves the memo on, so
        // ask again, as a client scrolling the file would.
        let mut answered = (0, 0);
        for _ in 0..1_000 {
            answered = hints(&mut state, &params(lines));
            if answered.0 > 0 && answered.1 > 0 {
                break;
            }
        }
        let (calls, types) = answered;
        assert!(
            calls > 0 && types > 0,
            "{} produced {calls} parameter and {types} type hints, so a row would time nothing",
            tier.name()
        );
        group.bench_function(BenchmarkId::new("screen", tier.name()), |b| {
            b.iter(|| hints(&mut state, &screen));
        });
        let file = params(lines);
        group.bench_function(BenchmarkId::new("file", tier.name()), |b| {
            b.iter(|| hints(&mut state, &file));
        });
    }
    group.finish();
}

criterion_group!(benches, inlay_hints);
criterion_main!(benches);
