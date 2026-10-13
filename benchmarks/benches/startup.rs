//! Startup and the `initialize` handshake.
//!
//! Budget: `initialize` under 100 ms, engine construction under 50 ms. These are
//! the numbers that decide whether the editor feels instant, so they are also
//! reported as percentiles — a p99 of 400 ms is a bad experience even with a
//! good mean.
//!
//! Process startup (fork/exec/dynamic linking) is not measurable from inside the
//! process; `scripts/bench-startup.sh` covers it.

use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use javelin_benchmarks::percentiles;
use javelin_core::Config;
use lsp_server::{Connection, Message, Request, RequestId};
use lsp_types::request::Request as _;

fn engine_construction(c: &mut Criterion) {
    c.bench_function("startup/engine", |b| {
        b.iter(|| javelin_server::ServerState::new(Config::default()).expect("engine"));
    });
}

/// One full handshake: connect, send `initialize`, receive capabilities, shut down.
fn one_handshake() -> Duration {
    let (client, server) = Connection::memory();
    let handle = std::thread::spawn(move || javelin_server::serve(&server, Config::default()));

    let started = Instant::now();
    client
        .sender
        .send(Message::Request(Request {
            id: RequestId::from(0),
            method: lsp_types::request::Initialize::METHOD.to_owned(),
            params: serde_json::json!({ "capabilities": {} }),
        }))
        .expect("send initialize");
    let _ = client.receiver.recv().expect("initialize response");
    let elapsed = started.elapsed();

    client
        .sender
        .send(Message::Request(Request {
            id: RequestId::from(1),
            method: lsp_types::request::Shutdown::METHOD.to_owned(),
            params: serde_json::Value::Null,
        }))
        .expect("send shutdown");
    let _ = client.receiver.recv();
    drop(client);
    let _ = handle.join();
    elapsed
}

fn initialize_handshake(c: &mut Criterion) {
    c.bench_function("startup/initialize", |b| b.iter(one_handshake));

    // Criterion reports central tendency; the budget is stated in percentiles,
    // so collect and print them explicitly.
    let mut samples: Vec<_> = (0..200).map(|_| one_handshake()).collect();
    if let Some(stats) = percentiles(&mut samples) {
        println!(
            "initialize latency: p50={:?} p95={:?} p99={:?} max={:?} (budget: p95 < 100ms)",
            stats.p50, stats.p95, stats.p99, stats.max
        );
    }
}

criterion_group!(benches, engine_construction, initialize_handshake);
criterion_main!(benches);
