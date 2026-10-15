//! The real binary, over real pipes.
//!
//! This exists because of a bug the in-memory integration tests structurally
//! cannot catch: `Connection::memory()` has no I/O threads, so a server that
//! holds the connection across `IoThreads::join()` passes every in-process test
//! and then hangs forever on `exit` — which the user sees as their editor
//! refusing to close.

mod harness;

use harness::{Server, no_classpath, own_cache};
use std::time::{Duration, Instant};

#[test]
fn a_full_session_over_stdio_starts_serves_and_exits_cleanly() {
    let mut server = Server::start();

    let capabilities = server.initialize(&no_classpath())["result"]["capabilities"].clone();
    assert_eq!(capabilities["positionEncoding"], "utf-16");
    assert_eq!(capabilities["textDocumentSync"]["change"], 2);

    server.send(&serde_json::json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));
    server.send(&serde_json::json!({
        "jsonrpc": "2.0", "method": "textDocument/didOpen",
        "params": { "textDocument": {
            "uri": "file:///A.java", "languageId": "java", "version": 1,
            "text": "class A { int x; }"
        }}
    }));
    server.send(&serde_json::json!({
        "jsonrpc": "2.0", "method": "textDocument/didChange",
        "params": {
            "textDocument": { "uri": "file:///A.java", "version": 2 },
            "contentChanges": [{
                "range": {
                    "start": { "line": 0, "character": 6 },
                    "end": { "line": 0, "character": 7 }
                },
                "text": "Renamed"
            }]
        }
    }));

    const RENAMED: &str = "```java\nclass Renamed\n```";
    let hover = server.eventually(
        "textDocument/hover",
        &serde_json::json!({
            "textDocument": { "uri": "file:///A.java" },
            "position": { "line": 0, "character": 6 }
        }),
        |answer| answer["result"]["contents"]["value"] == RENAMED,
    );
    assert_eq!(
        hover["result"]["contents"]["value"], RENAMED,
        "hover must catch up with the edited buffer, over real pipes"
    );

    server.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 90, "method": "textDocument/signatureHelp",
        "params": {
            "textDocument": { "uri": "file:///A.java" },
            "position": { "line": 0, "character": 6 }
        }
    }));
    assert_eq!(
        server.recv()["error"]["code"],
        -32601,
        "unimplemented, but answered"
    );

    server.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 3, "method": "shutdown", "params": null
    }));
    assert!(server.recv()["error"].is_null(), "shutdown must succeed");
    server.send(&serde_json::json!({ "jsonrpc": "2.0", "method": "exit", "params": null }));

    assert_eq!(
        server.wait_for_exit(Duration::from_secs(10)),
        Some(0),
        "the server must exit on `exit`, not hang holding the editor open"
    );
}

#[test]
fn a_client_that_vanishes_after_initialize_exits_cleanly() {
    let mut server = Server::start();
    let _ = server.initialize(&no_classpath());
    server.send(&serde_json::json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));

    // An editor that crashes never sends `shutdown`. The server must notice the
    // pipe closing and exit, rather than linger as an orphan process.
    server.close_stdin();
    assert_eq!(server.wait_for_exit(Duration::from_secs(10)), Some(0));
}

#[test]
fn a_client_that_vanishes_mid_handshake_is_not_reported_as_a_crash() {
    let mut server = Server::start();
    let _ = server.initialize(&no_classpath());

    // Disconnecting before `initialized` is the editor's problem, not ours.
    server.close_stdin();
    assert_eq!(server.wait_for_exit(Duration::from_secs(10)), Some(0));
}

/// A generated class large enough that parsing it is not instant.
fn large_java_source(methods: usize) -> String {
    let mut source = String::with_capacity(methods * 120);
    source.push_str("package com.example.generated;\n\npublic class Generated {\n");
    for i in 0..methods {
        source.push_str(&format!(
            "    private int field{i} = {i};\n\
             \n\
             \x20   public int method{i}(int argument) {{\n\
             \x20       int local = argument + field{i};\n\
             \x20       return local > {i} ? local * 2 : local;\n\
             \x20   }}\n\n"
        ));
    }
    source.push_str("}\n");
    source
}

/// The shutdown this task could break: reparses are queued on a worker pool, and
/// a pool that is joined while it still holds a sender, or that runs every
/// superseded job to completion first, hangs the editor on exit. Only the real
/// binary can show it — the in-memory connection has no I/O threads.
#[test]
fn a_session_with_edits_still_in_flight_shuts_down_promptly() {
    let mut server = Server::start();
    let _ = server.initialize(&no_classpath());
    server.send(&serde_json::json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));

    server.send(&serde_json::json!({
        "jsonrpc": "2.0", "method": "textDocument/didOpen",
        "params": { "textDocument": {
            "uri": "file:///Generated.java", "languageId": "java", "version": 1,
            "text": large_java_source(2_000)
        }}
    }));

    // A burst of edits, each superseding the reparse before it. Nothing waits
    // for any of them: by the time `shutdown` arrives the pool is still busy.
    for version in 2..=20 {
        server.send(&serde_json::json!({
            "jsonrpc": "2.0", "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": "file:///Generated.java", "version": version },
                "contentChanges": [{
                    "range": {
                        "start": { "line": 3, "character": 0 },
                        "end": { "line": 3, "character": 0 }
                    },
                    "text": "    // edit\n"
                }]
            }
        }));
    }

    server.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": null
    }));
    assert!(server.recv()["error"].is_null(), "shutdown must succeed");
    server.send(&serde_json::json!({ "jsonrpc": "2.0", "method": "exit", "params": null }));

    let started = Instant::now();
    assert_eq!(
        server.wait_for_exit(Duration::from_secs(10)),
        Some(0),
        "the server must join its parse pool and exit, not hang on the queue"
    );
    println!("exited {:?} after `exit`", started.elapsed());
}

/// The promise this task is most able to break: `initialize` is answered
/// before anything looks for a JDK, and the eleven-second `src.zip` read that
/// follows never gets between the editor and its first answer — or between the
/// user and closing the editor.
///
/// Only the real binary can show either half. The in-memory connection has no
/// I/O threads, and a background thread holding a `CacheDb` while the process
/// tries to exit is exactly the shape that hangs.
///
/// Needs a JDK on the machine: with none there is no read to still be running
/// at shutdown, and that half of this test would pass without checking
/// anything. It says so rather than passing quietly.
#[test]
fn startup_does_not_wait_for_the_classpath() {
    let (mut server, mut logs) = Server::start_with_logs();
    let (options, cache) = own_cache("startup");

    let answered = server.initialize(&options);
    let answered_at = Instant::now();
    assert!(answered["error"].is_null(), "initialize must succeed");
    assert!(
        !logs.saw("classpath"),
        "nothing may look for a JDK before `initialize` is answered: {:?}",
        logs.texts()
    );

    server.send(&serde_json::json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));

    // Specifically the line the job logs *before* opening the zip. Any
    // "classpath" line would also match "no JDK found", and then the read this
    // test wants in flight at shutdown would never have started.
    let reading = logs
        .wait_for("classpath: reading", Duration::from_secs(10))
        .unwrap_or_else(|| {
            panic!(
                "no JDK found, so there is no `src.zip` read to shut down during, \
                 and this test needs one. The server said: {:?}",
                logs.texts()
            )
        });
    assert!(
        reading > answered_at,
        "the classpath job must start after `initialize` was answered"
    );

    server.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": null
    }));
    assert!(server.recv()["error"].is_null(), "shutdown must succeed");
    // The half that would otherwise be vacuous. A debug-build read of a real
    // `src.zip` takes minutes, so the job is certainly still in it; if it
    // somehow finished, nothing was shut down during a read and this test
    // proved nothing.
    assert!(
        !logs.saw("classpath ready"),
        "the read finished before shutdown; nothing was shut down during one"
    );
    server.send(&serde_json::json!({ "jsonrpc": "2.0", "method": "exit", "params": null }));

    let exiting = Instant::now();
    let code = server.wait_for_exit(Duration::from_secs(10));
    // The job died with the process, mid-read, so this is usually the
    // directory `CacheDb::open` had not reached yet rather than a cache.
    let _ = std::fs::remove_dir_all(&cache);
    assert_eq!(
        code,
        Some(0),
        "the server must exit while the classpath read is still running"
    );
    println!("exited {:?} after `exit`", exiting.elapsed());
}
