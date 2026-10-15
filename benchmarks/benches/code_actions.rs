//! `textDocument/codeAction` latency with no `only`, as an editor asks on
//! every cursor move, across the generated fixture's tiers.
//!
//! Budget: p95 under 50 ms. The worst case is measured: fifty imports none
//! of which the file names, so "Remove unused imports" reads the whole file
//! rather than stopping once every name is seen.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use javelin_benchmarks::{Tier, java_source};
use javelin_core::{CancellationToken, Config};
use javelin_document::DocumentId;
use javelin_server::ServerState;
use lsp_types::CodeActionParams;

const URI: &str = "bench://Generated.java";

fn code_actions(c: &mut Criterion) {
    let mut group = c.benchmark_group("code_actions");
    let imports: String = (0..50)
        .map(|i| format!("import com.unused.Absent{i};\n"))
        .collect();
    let params: CodeActionParams = serde_json::from_value(serde_json::json!({
        "textDocument": { "uri": URI },
        "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
        "context": { "diagnostics": [] },
    }))
    .expect("well-formed params");
    for tier in Tier::ALL {
        let source = java_source(tier).replacen(";\n\n", &format!(";\n\n{imports}\n"), 1);
        let mut state = ServerState::new(Config::default()).expect("engine");
        state.did_open(DocumentId::new(URI), 1, source);
        let token = CancellationToken::new();
        let ask = |state: &mut ServerState| {
            javelin_server::handlers::code_action::handle(state, &params, &token)
                .expect("not cancelled")
        };
        assert_eq!(
            ask(&mut state).len(),
            1,
            "{} offered no removal to time",
            tier.name()
        );
        group.bench_function(BenchmarkId::new("unused_imports", tier.name()), |b| {
            b.iter(|| ask(&mut state));
        });
    }
    group.finish();
}

criterion_group!(benches, code_actions);
criterion_main!(benches);
