// A test-support module: later tasks add more test binaries on top of this
// plumbing, and each one compiles the whole module, so a helper unused by one
// binary is routine rather than dead code.
#![allow(dead_code)]

pub mod editor;
pub mod jdk;
pub mod text;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

/// One line the server logged, and when the test saw it.
struct LogLine {
    at: Instant,
    text: String,
}

/// Everything the server has logged so far, so an assertion can ask what
/// arrived and in which order.
pub struct Logs {
    arriving: Receiver<LogLine>,
    seen: Vec<LogLine>,
}

impl Logs {
    /// When a line containing `needle` arrived, waiting up to `within` for
    /// one. Lines seen while waiting are kept, so a later question about them
    /// still has an answer.
    pub fn wait_for(&mut self, needle: &str, within: Duration) -> Option<Instant> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(line) = self.seen.iter().find(|line| line.text.contains(needle)) {
                return Some(line.at);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.arriving.recv_timeout(left) {
                Ok(line) => self.seen.push(line),
                Err(_) => return None,
            }
        }
    }

    /// Whether a line containing `needle` has arrived already. Never waits.
    pub fn saw(&mut self, needle: &str) -> bool {
        self.wait_for(needle, Duration::ZERO).is_some()
    }

    pub fn texts(&self) -> Vec<&str> {
        self.seen.iter().map(|line| line.text.as_str()).collect()
    }
}

pub struct Server {
    child: Child,
    /// `Option` so a test can close the pipe early and still use the helpers.
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    /// Ids for [`Server::eventually`], kept clear of the ones tests spell out.
    next_id: i32,
    /// What the server sent unasked — `publishDiagnostics` — set aside by
    /// [`Server::recv`] so a response still pairs with its request.
    notifications: Vec<serde_json::Value>,
}

/// Settings for every test here that is not about the classpath.
///
/// Without it each one spends a thread reading whichever JDK the machine
/// running the suite has, and all of them race to write the one cache file
/// they share.
pub fn no_classpath() -> serde_json::Value {
    serde_json::json!({ "index": { "backgroundIndexing": false } })
}

/// Settings for the one test that *is* about the classpath: a cache of its
/// own, so it neither reads nor overwrites the developer's.
pub fn own_cache(name: &str) -> (serde_json::Value, PathBuf) {
    let dir = std::env::temp_dir().join(format!("javelin-{name}-{}", std::process::id()));
    let options = serde_json::json!({ "index": { "cacheDir": dir.to_string_lossy() } });
    (options, dir)
}

impl Server {
    pub fn start() -> Self {
        Self::launch_at("info", Stdio::null()).0
    }

    /// The same server, with its log stream timestamped as it arrives.
    ///
    /// Stderr must be *read* once it is piped: a full pipe would block the
    /// server on its next log line, which is not the thing under test.
    pub fn start_with_logs() -> (Self, Logs) {
        Self::start_with_logs_at("info")
    }

    /// A server whose log level is chosen by the caller.
    ///
    /// `parsed document` and `reparsed document` are `DEBUG`; the existing
    /// `info` default is enough for `classpath ready` and nothing else.
    pub fn start_with_logs_at(level: &str) -> (Self, Logs) {
        let (server, stderr) = Self::launch_at(level, Stdio::piped());
        let stderr = stderr.expect("piped stderr");
        let (lines, arriving) = channel();
        std::thread::spawn(move || {
            for text in BufReader::new(stderr).lines().map_while(Result::ok) {
                let line = LogLine {
                    at: Instant::now(),
                    text,
                };
                if lines.send(line).is_err() {
                    return;
                }
            }
        });
        (
            server,
            Logs {
                arriving,
                seen: Vec::new(),
            },
        )
    }

    /// `initialize`, under the settings this test wants the server started
    /// with, and the response.
    pub fn initialize(&mut self, options: &serde_json::Value) -> serde_json::Value {
        self.initialize_at(options, serde_json::Value::Null)
    }

    /// The same, with a `rootUri` — for a test about the workspace the client
    /// named rather than about the file it opened.
    pub fn initialize_at(
        &mut self,
        options: &serde_json::Value,
        root: serde_json::Value,
    ) -> serde_json::Value {
        self.initialize_as(options, root, serde_json::json!({}))
    }

    /// The same, from a client declaring `capabilities`.
    pub fn initialize_as(
        &mut self,
        options: &serde_json::Value,
        root: serde_json::Value,
        capabilities: serde_json::Value,
    ) -> serde_json::Value {
        self.send(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "processId": null, "rootUri": root, "capabilities": capabilities,
                "initializationOptions": options,
            }
        }));
        self.recv()
    }

    fn launch_at(level: &str, stderr: Stdio) -> (Self, Option<ChildStderr>) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_javelin"))
            .arg("serve")
            // Whatever the developer running the suite has in their shell must
            // not decide whether the server logs at all.
            .env("JAVELIN_LOG", level)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .expect("spawn javelin serve");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let logs = child.stderr.take();
        let server = Self {
            child,
            stdin: Some(stdin),
            stdout,
            next_id: 1_000,
            notifications: Vec::new(),
        };
        (server, logs)
    }

    pub fn send(&mut self, message: &serde_json::Value) {
        let body = serde_json::to_vec(message).expect("serialise");
        let stdin = self.stdin.as_mut().expect("stdin is still open");
        write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("write header");
        stdin.write_all(&body).expect("write body");
        stdin.flush().expect("flush");
    }

    /// Close the pipe, as an editor that crashed would.
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// The next response, setting aside every notification and request the
    /// server sends on the way.
    pub fn recv(&mut self) -> serde_json::Value {
        loop {
            let message = self.recv_any();
            if message.get("method").is_none() {
                return message;
            }
            self.notifications.push(message);
        }
    }

    /// Every notification set aside so far, in arrival order.
    pub fn notifications(&self) -> &[serde_json::Value] {
        &self.notifications
    }

    fn recv_any(&mut self) -> serde_json::Value {
        let mut length = None;
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("read header");
            assert!(read > 0, "server closed the stream before responding");
            let trimmed = line.trim();
            if trimmed.is_empty() {
                break;
            }
            if let Some(value) = trimmed.strip_prefix("Content-Length:") {
                length = value.trim().parse::<usize>().ok();
            }
        }
        let mut body = vec![0u8; length.expect("Content-Length header")];
        self.stdout.read_exact(&mut body).expect("read body");
        serde_json::from_slice(&body).expect("parse body")
    }

    /// Ask until the answer satisfies `settled`, then return it.
    ///
    /// Reparsing runs on a worker pool, so the request straight after an edit is
    /// answered from the previous view rather than made to wait. Catching up is
    /// the promise; the deadline fails a server that never does, not a slow
    /// machine.
    pub fn eventually(
        &mut self,
        method: &str,
        params: &serde_json::Value,
        settled: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.next_id += 1;
            self.send(&serde_json::json!({
                "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params
            }));
            let answer = self.recv();
            if settled(&answer) {
                return answer;
            }
            assert!(
                Instant::now() < deadline,
                "{method} never caught up with the buffer; last answer: {answer}"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Wait for the process to exit, killing it if it overruns.
    pub fn wait_for_exit(mut self, within: Duration) -> Option<i32> {
        let deadline = Instant::now() + within;
        loop {
            match self.child.try_wait().expect("try_wait") {
                Some(status) => return status.code(),
                None if Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    return None;
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}
