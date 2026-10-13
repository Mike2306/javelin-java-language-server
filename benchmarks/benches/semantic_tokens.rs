//! `textDocument/semanticTokens/full` latency, across the generated fixture's
//! tiers.
//!
//! The one feature in Phase 5a the plan asked for a benchmark on, because it
//! is the one whose cost is not obviously bounded: every other reading aid
//! answers from a bounded slice of the tree (a chain of ancestors, the
//! bindings visible at one offset), but this walk visits every token in the
//! file — the most nodes any handler in the server visits, per the module doc
//! on `javelin_parser::semantic_tokens`.
//!
//! Budget: p95 under 50 ms. Ruling 2 settles what happens on either side of
//! that number: this bench decides whether `semanticTokens/full/delta` is
//! owed, and a miss here is a recorded number and a `ponytail:` comment, not
//! a reason to build delta support in this slice.
//!
//! What it measured at `Tier::Large` and `Tier::Huge` before `ScopeTree`
//! indexed its bindings by name — 78.29 ms and 5.472 s at p95 — is what put
//! `MAX_DOCUMENT_BYTES` on the handler. The walk is linear now, so the guard
//! sits above `Tier::Large` and below `Tier::Huge`, and the `walk` rows time
//! the walk beneath it at every tier. The fixture's own sizes are asserted
//! below, so a generator that grew or shrank past the threshold fails the
//! bench rather than quietly moving which side of it a tier is on.
//!
//! [docs/performance.md]: ../../docs/performance.md
//!
//! Warm only, no `after_edit`: nothing here reparses, and `completion.rs`
//! already reports what a keystroke costs by way of the reparse and the view
//! rebuild it forces — repeating that measurement here would only restate it
//! under a different name.

use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use javelin_benchmarks::{Tier, java_source, percentiles};
use javelin_core::{CancellationToken, Config};
use javelin_document::DocumentId;
use javelin_server::ServerState;
use javelin_server::handlers::semantic_tokens::MAX_DOCUMENT_BYTES;
use lsp_types::{
    SemanticTokensParams, SemanticTokensRangeParams, SemanticTokensRangeResult,
    SemanticTokensResult,
};

const URI: &str = "bench://Generated.java";

fn opened(tier: Tier) -> ServerState {
    let mut state = ServerState::new(Config::default()).expect("engine");
    state.did_open(DocumentId::new(URI), 1, java_source(tier));
    state
}

fn params() -> SemanticTokensParams {
    serde_json::from_value(serde_json::json!({
        "textDocument": { "uri": URI },
    }))
    .expect("well-formed params")
}

/// One request, JSON included — the client waits for that too.
///
/// Returns the answer so a caller can assert it actually produced tokens: a
/// fixture regression that silently made the walk answer empty would
/// otherwise still produce a fast, plausible-looking number, which is exactly
/// the failure `definition.rs`'s own `define` helper exists to close for that
/// bench.
fn tokens(state: &mut ServerState, params: &SemanticTokensParams) -> Option<SemanticTokensResult> {
    let answer =
        javelin_server::handlers::semantic_tokens::handle(state, params, &CancellationToken::new())
            .expect("not cancelled");
    let _ = serde_json::to_value(&answer).expect("serialisable");
    answer
}

/// A screen of sixty lines just above the file's end — the bottom of the
/// generated class, so the walk scans every sibling before it.
fn screen(tier: Tier) -> SemanticTokensRangeParams {
    let lines = java_source(tier).lines().count() as u32;
    serde_json::from_value(serde_json::json!({
        "textDocument": { "uri": URI },
        "range": {
            "start": { "line": lines.saturating_sub(61), "character": 0 },
            "end": { "line": lines.saturating_sub(1), "character": 0 }
        }
    }))
    .expect("well-formed params")
}

/// One range request, JSON included, and how many tokens it answered.
fn ranged(state: &mut ServerState, params: &SemanticTokensRangeParams) -> usize {
    let answer = javelin_server::handlers::semantic_tokens::handle_range(
        state,
        params,
        &CancellationToken::new(),
    )
    .expect("not cancelled");
    let _ = serde_json::to_value(&answer).expect("serialisable");
    match answer {
        Some(SemanticTokensRangeResult::Tokens(tokens)) => tokens.data.len(),
        _ => 0,
    }
}

fn token_count(answer: &Option<SemanticTokensResult>) -> usize {
    match answer {
        Some(SemanticTokensResult::Tokens(tokens)) => tokens.data.len(),
        _ => 0,
    }
}

fn semantic_tokens_full(c: &mut Criterion) {
    let mut group = c.benchmark_group("semantic_tokens");
    for tier in Tier::ALL {
        let mut state = opened(tier);
        let request = params();
        let bytes = java_source(tier).len();
        let count = token_count(&tokens(&mut state, &request));
        if bytes > MAX_DOCUMENT_BYTES {
            assert_eq!(
                count,
                0,
                "{} is {bytes} bytes, past the {MAX_DOCUMENT_BYTES}-byte guard, \
                 so this row times the guard and not the walk",
                tier.name()
            );
        } else {
            assert!(
                count > 0,
                "{} is {bytes} bytes, inside the guard, and produced no tokens",
                tier.name()
            );
        }
        group.bench_function(BenchmarkId::new("full", tier.name()), |b| {
            b.iter(|| tokens(&mut state, &request));
        });
        // Every tier is coloured by range, the guarded one included.
        let screen = screen(tier);
        assert!(
            ranged(&mut state, &screen) > 0,
            "{}'s screen produced no tokens",
            tier.name()
        );
        group.bench_function(BenchmarkId::new("range", tier.name()), |b| {
            b.iter(|| ranged(&mut state, &screen));
        });
    }
    group.finish();

    // Same rule `definition.rs` and `completion.rs` follow: `cargo bench`
    // passes `--bench` and `cargo test` does not, and the loop below would
    // otherwise run unbounded on every CI job.
    if !std::env::args().any(|argument| argument == "--bench") {
        return;
    }

    for tier in Tier::ALL {
        let mut state = opened(tier);
        let request = params();
        let mut samples: Vec<Duration> = (0..200)
            .map(|_| {
                let started = Instant::now();
                tokens(&mut state, &request);
                started.elapsed()
            })
            .collect();
        report(tier, &mut samples);
        let screen = screen(tier);
        let mut samples: Vec<Duration> = (0..200)
            .map(|_| {
                let started = Instant::now();
                ranged(&mut state, &screen);
                started.elapsed()
            })
            .collect();
        if let Some(stats) = percentiles(&mut samples) {
            println!(
                "semantic_tokens range {} (60 lines at the end): p50={:?} p95={:?} max={:?} \
                 (budget: p95 < 50ms)",
                tier.name(),
                stats.p50,
                stats.p95,
                stats.max
            );
        }
    }

    // The walk itself, beneath the handler's size guard, at every tier —
    // what says whether `MAX_DOCUMENT_BYTES` can move. Twenty samples rather
    // than two hundred: before `ScopeTree` indexed its bindings by name one
    // `Tier::Huge` walk took over five seconds.
    for tier in Tier::ALL {
        let document =
            javelin_document::TextDocument::new(DocumentId::new(URI), 1, java_source(tier));
        let tree = javelin_parser::JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let scopes = javelin_parser::ScopeTree::build(&document, &tree);
        let syntax = javelin_parser::FileSyntax::build(&document, &tree);
        let token = CancellationToken::new();
        let mut samples: Vec<Duration> = (0..20)
            .map(|_| {
                let started = Instant::now();
                let walked =
                    javelin_parser::semantic_tokens(&document, &tree, &scopes, &syntax, &token)
                        .expect("not cancelled");
                assert!(!walked.is_empty(), "{} produced no tokens", tier.name());
                started.elapsed()
            })
            .collect();
        if let Some(stats) = percentiles(&mut samples) {
            println!(
                "semantic_tokens walk {}: p50={:?} p95={:?} max={:?} (unguarded)",
                tier.name(),
                stats.p50,
                stats.p95,
                stats.max
            );
        }
    }
}

fn report(tier: Tier, samples: &mut [Duration]) {
    if let Some(stats) = percentiles(samples) {
        println!(
            "semantic_tokens full {}: p50={:?} p95={:?} p99={:?} max={:?} (budget: p95 < 50ms)",
            tier.name(),
            stats.p50,
            stats.p95,
            stats.p99,
            stats.max
        );
    }
}

criterion_group!(benches, semantic_tokens_full);
criterion_main!(benches);
