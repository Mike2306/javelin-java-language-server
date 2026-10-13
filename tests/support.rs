//! Shared helpers for the integration tests.
//!
//! These tests exercise real crate boundaries: a genuine LSP handshake over an
//! in-memory connection, and the document/parser pipeline end to end. Anything
//! that only tests one crate belongs in that crate's own `#[cfg(test)]` module.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::Notification as _;
use lsp_types::request::Request as _;

/// A live server on a background thread, driven from the client half of an
/// in-memory connection.
///
/// A real handshake and a real request loop, which is what separates these tests
/// from the in-process ones: everything a client can send has to survive
/// serialisation and routing to get here.
pub struct Client {
    connection: Option<Connection>,
    server: Option<thread::JoinHandle<javelin_core::Result<()>>>,
}

// `lsp_server::Connection` is opaque; the only thing worth printing is whether
// this client still has one.
impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("connected", &self.connection.is_some())
            .finish()
    }
}

/// The defaults, minus the background classpath read.
///
/// These tests assert what one open buffer resolves to, and none of them
/// installs a classpath. Leaving the read on would have every one of them
/// spend a worker on the machine's own JDK — and make what they assert depend
/// on which JDK that is.
fn config() -> javelin_core::Config {
    let mut config = javelin_core::Config::default();
    config.index.background_indexing = false;
    config
}

impl Client {
    /// Start a server and complete `initialize`/`initialized`.
    pub fn started() -> (Self, serde_json::Value) {
        Client::launch(serde_json::Value::Null)
    }

    /// Start a server, complete the handshake and discard the capabilities.
    pub fn ready() -> Self {
        Client::started().0
    }

    /// The same, with `initializationOptions` the client sends.
    ///
    /// For the one test that needs the background classpath read, which the
    /// default configuration here turns off. The options *replace* the
    /// server's configuration rather
    /// than merging into it, so anything this does not name is the default —
    /// including the read being on again.
    pub fn with_options(options: serde_json::Value) -> Self {
        Client::launch(options).0
    }

    fn launch(options: serde_json::Value) -> (Self, serde_json::Value) {
        let (connection, server) = Connection::memory();
        let server = thread::spawn(move || javelin_server::serve(&server, config()));
        let mut client = Client {
            connection: Some(connection),
            server: Some(server),
        };
        let capabilities = client.initialize(options);
        (client, capabilities)
    }

    fn connection(&self) -> &Connection {
        self.connection.as_ref().expect("the client is still open")
    }

    fn initialize(&mut self, options: serde_json::Value) -> serde_json::Value {
        let mut params =
            serde_json::json!({ "processId": null, "rootUri": null, "capabilities": {} });
        if !options.is_null() {
            params["initializationOptions"] = options;
        }
        let response = self.request(0, lsp_types::request::Initialize::METHOD, params);
        self.notify(
            lsp_types::notification::Initialized::METHOD,
            serde_json::json!({}),
        );
        response
            .response_result
            .expect("initialize returned an error")
    }

    pub fn notify(&self, method: &str, params: serde_json::Value) {
        self.connection()
            .sender
            .send(Message::Notification(Notification {
                method: method.to_owned(),
                params,
            }))
            .unwrap_or_else(|err| panic!("send {method}: {err}"));
    }

    pub fn send(&self, id: i32, method: &str, params: serde_json::Value) {
        self.connection()
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(id),
                method: method.to_owned(),
                params,
            }))
            .unwrap_or_else(|err| panic!("send {method}: {err}"));
    }

    pub fn receive(&self) -> Message {
        self.connection()
            .receiver
            .recv()
            .expect("the server stopped answering")
    }

    pub fn request(&self, id: i32, method: &str, params: serde_json::Value) -> Response {
        self.send(id, method, params);
        match self.receive() {
            Message::Response(response) => response,
            other => panic!("expected a response to {method}, got {other:?}"),
        }
    }

    /// Send a request and unwrap its successful result.
    pub fn ask(&self, id: i32, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.request(id, method, params)
            .response_result
            .unwrap_or_else(|err| panic!("{method} answered an error: {err:?}"))
    }

    pub fn did_open(&self, uri: &str, text: &str) {
        self.notify(
            lsp_types::notification::DidOpenTextDocument::METHOD,
            serde_json::json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "java",
                    "version": 1,
                    "text": text,
                }
            }),
        );
    }

    /// Ask until the answer satisfies `settled`, then return it.
    ///
    /// A reparse runs on a worker, so the request straight after an edit is
    /// answered from the previous view — deliberately, rather than by making the
    /// loop wait for the pool. What the client is promised is that it catches
    /// up, so that is what a test can assert. The deadline is generous: this
    /// fails when the server never converges, not when the machine is busy.
    pub fn eventually(
        &self,
        method: &str,
        params: serde_json::Value,
        settled: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        static NEXT_ID: AtomicI32 = AtomicI32::new(100_000);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let answer = self.ask(id, method, params.clone());
            if settled(&answer) {
                return answer;
            }
            assert!(
                Instant::now() < deadline,
                "{method} never caught up with the buffer; last answer: {answer}"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Shut the server down and assert it exited cleanly.
    ///
    /// Explicit rather than in `Drop`: a test that forgets it should hang
    /// visibly, not pass while the assertion is skipped during a panic unwind.
    pub fn shutdown(mut self) {
        self.send(
            9999,
            lsp_types::request::Shutdown::METHOD,
            serde_json::Value::Null,
        );
        let _ = self.receive();
        drop(self.connection.take());
        self.server
            .take()
            .expect("started once")
            .join()
            .expect("server thread panicked")
            .expect("server exited with an error");
    }
}

/// Absolute path to `tests/fixtures`.
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Read a fixture by name, e.g. `fixture("java/Simple.java")`.
pub fn fixture(relative: &str) -> String {
    let path = fixtures_dir().join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("missing fixture {}: {err}", path.display()))
}

/// One line per declaration, nested by indentation.
///
/// Asserting a whole outline against an expected block keeps the shape of a file
/// readable in the test; a hundred struct literals assert the same thing and
/// hide it. Modifiers and spans are deliberately absent — the tests that care
/// about those assert on them directly.
pub fn outline(view: &javelin_parser::FileSyntax) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    if let Some(package) = &view.package {
        let _ = writeln!(out, "package {}", package.name);
    }
    for import in &view.imports {
        let modifier = if import.is_static { "static " } else { "" };
        let suffix = if import.is_on_demand { ".*" } else { "" };
        let _ = writeln!(out, "import {modifier}{}{suffix}", import.name);
    }
    render(&view.declarations, 0, &mut out);
    out
}

fn render(decls: &[javelin_parser::Decl], depth: usize, out: &mut String) {
    use javelin_parser::DeclKind;
    use std::fmt::Write as _;

    for decl in decls {
        let _ = write!(out, "{}{}", "  ".repeat(depth), kind(decl.kind));
        if !decl.name.is_empty() {
            let _ = write!(out, " {}", decl.name);
        }
        if let Some(parameters) = &decl.type_parameters {
            out.push_str(parameters);
        }
        if matches!(decl.kind, DeclKind::Method | DeclKind::Constructor) {
            let rendered: Vec<_> = decl
                .parameters
                .iter()
                .map(|p| {
                    let dots = if p.varargs { "..." } else { "" };
                    format!(
                        "{}: {}{dots}",
                        p.name,
                        p.declared_type.as_deref().unwrap_or("?")
                    )
                })
                .collect();
            let _ = write!(out, "({})", rendered.join(", "));
        }
        if let Some(declared) = &decl.declared_type {
            let _ = write!(out, ": {declared}");
        }
        if !decl.throws.is_empty() {
            let _ = write!(out, " throws {}", decl.throws.join(", "));
        }
        if decl.doc.is_some() {
            out.push_str(" [doc]");
        }
        out.push('\n');
        render(&decl.children, depth + 1, out);
    }
}

fn kind(kind: javelin_parser::DeclKind) -> &'static str {
    use javelin_parser::DeclKind::*;
    match kind {
        Class => "class",
        Interface => "interface",
        Enum => "enum",
        Record => "record",
        AnnotationType => "@interface",
        AnonymousClass => "anonymous",
        Field => "field",
        Method => "method",
        Constructor => "constructor",
        StaticInitializer => "static-init",
        InstanceInitializer => "instance-init",
        EnumConstant => "constant",
        RecordComponent => "component",
        AnnotationElement => "element",
        Parameter => "param",
        LocalVariable => "local",
    }
}
