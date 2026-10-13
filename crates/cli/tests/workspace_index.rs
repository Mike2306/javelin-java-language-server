//! The workspace's own sources, indexed by the real binary over real pipes.
//!
//! Nothing on the request path reads this index yet, so there is no answer to
//! assert on and no row in the editor pass. What a test can see is what the
//! server says it indexed, which is the whole observable surface of this
//! slice: the walk ran, it found what the workspace holds, and it found it
//! after the handshake rather than during it.

mod harness;

use harness::Server;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A workspace under the temp directory, removed when the test ends.
struct Workspace(PathBuf);

impl Workspace {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("javelin-workspace-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a temp workspace");
        Self(path)
    }

    fn file(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("dirs");
        std::fs::write(path, text).expect("write");
    }

    /// Settings that keep this test off the developer's caches and off
    /// whichever JDK the machine has: a cache root of its own, and a
    /// `jdk.home` that is not a JDK, which the classpath job refuses to fall
    /// back from. Background indexing stays *on* — it is what is under test.
    fn options(&self) -> serde_json::Value {
        serde_json::json!({
            "index": { "cacheDir": self.0.join("cache").to_string_lossy() },
            "jdk": { "home": self.0.join("not-a-jdk").to_string_lossy() },
        })
    }

    fn uri(&self) -> serde_json::Value {
        serde_json::Value::String(uri(&self.0))
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A `file:` URI for a path this machine holds. Percent-encodes everything
/// outside the unreserved set, because a temp directory under a profile with a
/// space in its name is an ordinary machine.
fn uri(path: &Path) -> String {
    let path = path.to_str().expect("a UTF-8 temp path");
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                encoded.push(char::from(byte));
            }
            b'\\' => encoded.push('/'),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    let root = if encoded.starts_with('/') { "" } else { "/" };
    format!("file://{root}{encoded}")
}

#[test]
fn the_workspaces_own_sources_are_indexed_after_the_handshake_and_not_before() {
    let workspace = Workspace::new("indexed");
    workspace.file(
        "src/main/java/com/example/A.java",
        "package com.example;\nclass A {}",
    );
    workspace.file(
        "src/main/java/com/example/B.java",
        "package com.example;\nclass B { class Inner {} }",
    );
    // A build output directory holds a copy of what `src` declares. Counting
    // it would be counting `com.example.A` twice.
    workspace.file(
        "target/classes/com/example/A.java",
        "package com.example;\nclass A {}",
    );

    let (mut server, mut logs) = Server::start_with_logs();
    let answered = server.initialize_at(&workspace.options(), workspace.uri());
    assert!(answered["error"].is_null(), "initialize must succeed");
    assert!(
        !logs.saw("workspace"),
        "nothing may walk the workspace before `initialize` is answered: {:?}",
        logs.texts()
    );

    server.send(&serde_json::json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));
    assert!(
        logs.wait_for("workspace ready", Duration::from_secs(30))
            .is_some(),
        "the walk never finished: {:?}",
        logs.texts()
    );

    let line = logs
        .texts()
        .into_iter()
        .find(|text| text.contains("workspace ready"))
        .expect("the line just waited for")
        .to_owned();
    assert!(line.contains("files=2"), "two source files, not: {line}");
    assert!(
        line.contains("types=3"),
        "two top-level types and one nested, not: {line}"
    );

    finish(server);
}

#[test]
fn a_client_that_names_no_folder_gets_no_index_and_no_error() {
    let workspace = Workspace::new("no-folder");
    let (mut server, mut logs) = Server::start_with_logs();

    let answered = server.initialize(&workspace.options());
    assert!(answered["error"].is_null(), "initialize must succeed");
    server.send(&serde_json::json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));

    assert!(
        logs.wait_for("named no folder", Duration::from_secs(10))
            .is_some(),
        "a folderless session must say so rather than walking something: {:?}",
        logs.texts()
    );
    assert!(!logs.saw("workspace ready"), "there was nothing to index");

    finish(server);
}

/// `shutdown`, then `exit`, then the process is gone — the sequence an editor
/// closing a window sends, and the one a background thread must not hold up.
fn finish(mut server: Server) {
    server.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": null
    }));
    assert!(server.recv()["error"].is_null(), "shutdown must succeed");
    server.send(&serde_json::json!({ "jsonrpc": "2.0", "method": "exit", "params": null }));
    assert_eq!(
        server.wait_for_exit(Duration::from_secs(10)),
        Some(0),
        "the server must exit cleanly with an index in hand"
    );
}
