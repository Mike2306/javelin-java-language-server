//! How long the request loop takes to answer while a huge file is reparsing.
//!
//! This is the number the parse pool exists for. A `didChange` on a
//! `Tier::Huge` buffer costs a few hundred milliseconds of parsing and view
//! building; the question is whether the request that arrives immediately
//! afterwards waits for it. Before the pool it did, because the loop did the
//! reparse itself, and this measurement *was* the reparse.
//!
//! Measured over a real `serve` loop and a real (in-memory) connection rather
//! than against `ServerState` directly, because the thing being measured is the
//! loop: a state method cannot tell you what the loop was busy with.
//!
//! Hover rather than the outline, because a 20 000-method `documentSymbol`
//! payload costs more to serialise than the loop costs to reach, and that is
//! not the question here.
//!
//! The answer during that window is the previous view, deliberately, so the
//! one assertion here is that a real hover comes back at all — a fast `null`
//! would otherwise score like a fast answer. It runs once, outside the timer.
//! That the versions inside the view agree is asserted in `javelin-server`'s
//! own tests; everything else in this file only times things.

use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use javelin_benchmarks::{Tier, java_source, percentiles};
use javelin_core::Config;
use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::notification::Notification as _;
use lsp_types::request::Request as _;

const URI: &str = "bench://Generated.java";

/// A client, a server thread, and one huge file already open.
struct Session {
    /// `Option` so [`Drop`] can close the connection before joining the server.
    client: Option<Connection>,
    server: Option<std::thread::JoinHandle<javelin_core::Result<()>>>,
    version: i32,
    id: i32,
}

impl Session {
    fn open(tier: Tier) -> Self {
        let (client, server) = Connection::memory();
        let server = std::thread::spawn(move || javelin_server::serve(&server, Config::default()));
        let mut session = Self {
            client: Some(client),
            server: Some(server),
            version: 1,
            id: 0,
        };
        let _ = session.request(
            lsp_types::request::Initialize::METHOD,
            serde_json::json!({ "processId": null, "rootUri": null, "capabilities": {} }),
        );
        session.notify(
            lsp_types::notification::Initialized::METHOD,
            serde_json::json!({}),
        );
        session.notify(
            lsp_types::notification::DidOpenTextDocument::METHOD,
            serde_json::json!({ "textDocument": {
                "uri": URI, "languageId": "java", "version": 1, "text": java_source(tier)
            }}),
        );
        session
    }

    fn connection(&self) -> &Connection {
        self.client.as_ref().expect("still connected")
    }

    fn notify(&self, method: &str, params: serde_json::Value) {
        self.connection()
            .sender
            .send(Message::Notification(Notification {
                method: method.to_owned(),
                params,
            }))
            .expect("send");
    }

    /// Send a request and return what came back, so a caller can check that a
    /// fast answer is an answer at all.
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.id += 1;
        self.connection()
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(self.id),
                method: method.to_owned(),
                params,
            }))
            .expect("send");
        match self.connection().receiver.recv().expect("a response") {
            Message::Response(response) => response
                .response_result
                .unwrap_or_else(|err| panic!("{method} answered an error: {err:?}")),
            other => panic!("expected a response to {method}, got {other:?}"),
        }
    }

    /// One keystroke, then the request an editor sends straight after it. Timed
    /// from the notification, so a loop that reparses inline is timed doing it.
    ///
    /// The edit goes in below the probe — `line 2` is `public class Generated {`
    /// and the hover is on its name — so the position stays valid however many
    /// times this runs, and the answer stays checkable.
    fn edit_then_ask(&mut self) -> (Duration, serde_json::Value) {
        self.version += 1;
        let started = Instant::now();
        self.notify(
            lsp_types::notification::DidChangeTextDocument::METHOD,
            serde_json::json!({
                "textDocument": { "uri": URI, "version": self.version },
                "contentChanges": [{
                    "range": {
                        "start": { "line": 4, "character": 0 },
                        "end": { "line": 4, "character": 0 }
                    },
                    "text": "        // edit\n"
                }]
            }),
        );
        let answer = self.request(
            lsp_types::request::HoverRequest::METHOD,
            serde_json::json!({
                "textDocument": { "uri": URI },
                "position": { "line": 2, "character": 15 },
            }),
        );
        (started.elapsed(), answer)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.request(
            lsp_types::request::Shutdown::METHOD,
            serde_json::Value::Null,
        );
        // Same rule as `run_stdio`: the server only stops once every sender is
        // gone, so the connection goes before the join.
        drop(self.client.take());
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

fn hover_during_reparse(c: &mut Criterion) {
    // Outside the closure: criterion calls it several times, and each call
    // would otherwise spawn a server and parse 20 000 methods from scratch.
    let mut session = Session::open(Tier::Huge);
    // Once, and outside the timer, because a server that answered a fast `null`
    // would score exactly like one that answered. A hover of the class name is
    // the same either side of a reparse, so this holds whether the answer comes
    // from the pool's result or from the view it has not replaced yet.
    let (_, answer) = session.edit_then_ask();
    assert!(
        !answer["contents"]["value"].is_null(),
        "the timed request must produce a real answer, got {answer}"
    );

    let mut group = c.benchmark_group("loop");
    // Each iteration edits a 20 000-method file; ten samples is plenty, and it
    // keeps this off the critical path of a plain `cargo test --all-targets`.
    group.sample_size(10);
    group.bench_function("hover_during_huge_reparse", |b| {
        b.iter(|| session.edit_then_ask().0);
    });
    group.finish();

    // `cargo bench` passes `--bench` and `cargo test` does not; the loop below
    // is plain code that would otherwise run on every CI job.
    if !std::env::args().any(|argument| argument == "--bench") {
        return;
    }

    let mut samples: Vec<_> = (0..200).map(|_| session.edit_then_ask().0).collect();
    if let Some(stats) = percentiles(&mut samples) {
        println!(
            "hover after a huge edit: p50={:?} p95={:?} p99={:?} max={:?} \
             (the reparse it does not wait for is ~300 ms)",
            stats.p50, stats.p95, stats.p99, stats.max
        );
    }
}

criterion_group!(benches, hover_during_reparse);
criterion_main!(benches);
