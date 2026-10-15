//! A client that obeys the handshake.
//!
//! Every in-process test calls a handler directly, so a feature can be correct
//! and unreachable at the same time — which is exactly what shipped in Phase
//! 2a, when `capabilities.rs` advertised no trigger characters and all 388
//! tests passed against member completion no editor could invoke. This editor
//! refuses to send what the handshake did not permit, so that bug fails at the
//! point of *asking*.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::harness::text::{offset_of, position_of};
use crate::harness::{Logs, Server};

/// The absolute path to the checked-in demo project.
pub fn demo_project() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/projects/zed-demo")
        .canonicalize()
        .expect("the demo project is checked in")
}

/// A copy of the demo project under the temp directory, for a check that
/// writes files: the checked-in project is never written to. The copy is
/// fresh for every call and left for the OS to clean up.
pub fn scratch_project(name: &str) -> PathBuf {
    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).expect("scratch directory");
        for entry in std::fs::read_dir(from).expect("readable").flatten() {
            let target = to.join(entry.file_name());
            if entry.file_type().expect("a type").is_dir() {
                copy(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).expect("copy");
            }
        }
    }
    let root = std::env::temp_dir().join(format!("javelin-scratch-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    copy(&demo_project(), &root);
    root
}

/// A `file:` URI for an absolute path.
///
/// `format!("file://{}", path.display())` is wrong twice. A canonicalised
/// Windows path is `\\?\C:\…`, which is not a URI at all, so `lsp-types`
/// refuses to deserialise the notification and the server drops it silently —
/// on a CI matrix that includes `windows-latest`, that is every editor test
/// failing for a reason none of them names. And on Unix a checkout directory
/// containing a space or a `#` breaks the naive form too.
//
// ponytail: encodes the four characters a checkout path plausibly holds rather
// than the grammar of RFC 8089. The upgrade is the `url` crate, which this
// workspace does not depend on and which one test helper does not justify.
pub fn file_uri(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let raw = raw.strip_prefix(r"\\?\").unwrap_or(&raw).replace('\\', "/");
    let mut uri = String::from("file://");
    if !raw.starts_with('/') {
        uri.push('/');
    }
    for ch in raw.chars() {
        match ch {
            '%' => uri.push_str("%25"),
            ' ' => uri.push_str("%20"),
            '#' => uri.push_str("%23"),
            '?' => uri.push_str("%3F"),
            _ => uri.push(ch),
        }
    }
    uri
}

struct Mirror {
    text: String,
    version: i32,
}

pub struct Editor {
    server: Server,
    logs: Logs,
    /// The `initialize` result as it arrived. Deliberately not deserialised
    /// into `lsp_types::ServerCapabilities`: a capability that stops being
    /// advertised must read as *absent*, and an `Option` field that defaults to
    /// `None` reads the same as "not applicable".
    capabilities: serde_json::Value,
    documents: HashMap<String, Mirror>,
}

impl Editor {
    pub fn launch(options: &serde_json::Value) -> Self {
        Self::launch_at("info", options)
    }

    /// The same, logging at `debug` — which is where `parsed document` and
    /// `reparsed document` live.
    pub fn launch_verbose(options: &serde_json::Value) -> Self {
        Self::launch_at("debug", options)
    }

    /// A verbose editor that also names a workspace root, for a check about
    /// what the background walk found rather than about the open buffer.
    pub fn launch_verbose_in(options: &serde_json::Value, root: &Path) -> Self {
        Self::launch_at_root("debug", options, serde_json::Value::String(file_uri(root)))
    }

    fn launch_at(level: &str, options: &serde_json::Value) -> Self {
        Self::launch_at_root(level, options, serde_json::Value::Null)
    }

    fn launch_at_root(level: &str, options: &serde_json::Value, root: serde_json::Value) -> Self {
        let (mut server, logs) = Server::start_with_logs_at(level);
        // As Zed and VS Code do: the server then asks to watch the files, and
        // to create a token for each stretch of background progress —
        // requests this editor sets aside unanswered.
        let answer = server.initialize_as(
            options,
            root,
            serde_json::json!({
                "workspace": { "didChangeWatchedFiles": { "dynamicRegistration": true } },
                "window": { "workDoneProgress": true }
            }),
        );
        let capabilities = answer["result"]["capabilities"].clone();
        assert!(
            capabilities.is_object(),
            "the handshake answered no capabilities: {answer}"
        );
        server.send(&serde_json::json!({
            "jsonrpc": "2.0", "method": "initialized", "params": {}
        }));
        Self {
            server,
            logs,
            capabilities,
            documents: HashMap::new(),
        }
    }

    pub fn logs(&mut self) -> &mut Logs {
        &mut self.logs
    }

    pub fn server(&mut self) -> &mut Server {
        &mut self.server
    }

    pub fn capabilities(&self) -> &serde_json::Value {
        &self.capabilities
    }

    pub fn open(&mut self, path: &Path) -> String {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        let uri = file_uri(path);
        self.server.send(&serde_json::json!({
            "jsonrpc": "2.0", "method": "textDocument/didOpen",
            "params": { "textDocument": {
                "uri": uri, "languageId": "java", "version": 0, "text": text
            }}
        }));
        self.documents
            .insert(uri.clone(), Mirror { text, version: 0 });
        uri
    }

    pub fn text(&self, uri: &str) -> &str {
        &self.documents.get(uri).expect("an opened document").text
    }

    pub fn version(&self, uri: &str) -> i32 {
        self.documents.get(uri).expect("an opened document").version
    }

    /// The position just past the first occurrence of `needle`, searched
    /// file-wide in the editor's own mirror — so a needle must be unique
    /// enough to name the place, and a `type_at` before the search moves it.
    ///
    /// Tests name a position by the text it follows rather than by a column, so
    /// an edit to the demo project shifts a check rather than silently
    /// invalidating it.
    pub fn find(&self, uri: &str, needle: &str) -> (u32, u32) {
        let text = self.text(uri);
        let at = text
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} is not in {uri}"));
        position_of(text, at + needle.len())
    }

    /// Type `text`, one `didChange` per character, as a person does.
    ///
    /// One message per character is the point: it is the traffic shape a fast
    /// typist produces, and it is what exercises incremental reparsing and the
    /// pool's supersede path.
    pub fn type_at(&mut self, uri: &str, line: u32, character: u32, text: &str) {
        assert_eq!(
            self.capabilities["textDocumentSync"]["change"],
            serde_json::json!(2),
            "this editor only knows how to send incremental changes"
        );
        let mut at = {
            let mirror = self.documents.get(uri).expect("an opened document");
            offset_of(&mirror.text, line, character)
        };
        for ch in text.chars() {
            let mirror = self.documents.get_mut(uri).expect("an opened document");
            let (l, c) = position_of(&mirror.text, at);
            mirror.text.insert(at, ch);
            mirror.version += 1;
            let version = mirror.version;
            at += ch.len_utf8();
            self.server.send(&serde_json::json!({
                "jsonrpc": "2.0", "method": "textDocument/didChange",
                "params": {
                    "textDocument": { "uri": uri, "version": version },
                    "contentChanges": [{
                        "range": {
                            "start": { "line": l, "character": c },
                            "end": { "line": l, "character": c }
                        },
                        "text": ch.to_string()
                    }]
                }
            }));
        }
    }

    /// Whether a client that obeys this handshake would ask for completion here.
    ///
    /// A real client asks after an identifier character, and otherwise only
    /// after a character the server advertised. Anything else is a request no
    /// editor sends, so a test that made it would be testing a handler nobody
    /// can reach.
    fn may_complete(&self, uri: &str, line: u32, character: u32) -> Result<(), String> {
        let text = self.text(uri);
        let before = text[..offset_of(text, line, character)]
            .chars()
            .next_back()
            .ok_or_else(|| "nothing precedes this position".to_string())?;
        if before.is_alphanumeric() || before == '_' || before == '$' {
            return Ok(());
        }
        let triggers: Vec<String> = self.capabilities["completionProvider"]["triggerCharacters"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let typed = before.to_string();
        if triggers.contains(&typed) {
            return Ok(());
        }
        Err(format!(
            "a client obeying this handshake would not ask for completion after {before:?}; \
             advertised trigger characters: {triggers:?}"
        ))
    }

    /// Completion labels, or why a real client would not have asked.
    pub fn try_completions_at(
        &mut self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<String>, String> {
        self.may_complete(uri, line, character)?;
        let answer = self.settled("textDocument/completion", uri, line, character);
        let items = match &answer["result"] {
            serde_json::Value::Array(items) => items.clone(),
            other => other["items"].as_array().cloned().unwrap_or_default(),
        };
        let mut labels: Vec<String> = items
            .iter()
            .filter_map(|item| item["label"].as_str().map(str::to_string))
            .collect();
        labels.sort();
        Ok(labels)
    }

    /// The same, failing the test when a real client would not have asked.
    pub fn completions_at(&mut self, uri: &str, line: u32, character: u32) -> Vec<String> {
        self.try_completions_at(uri, line, character)
            .unwrap_or_else(|why| panic!("{why}"))
    }

    pub fn definition_at(&mut self, uri: &str, line: u32, character: u32) -> Option<(String, u32)> {
        assert!(
            !self.capabilities["definitionProvider"].is_null(),
            "definition is not advertised, so no client asks for it"
        );
        let answer = self.settled("textDocument/definition", uri, line, character);
        let location = match &answer["result"] {
            serde_json::Value::Array(locations) => locations.first()?.clone(),
            serde_json::Value::Null => return None,
            one => one.clone(),
        };
        let target = location["uri"].as_str()?.to_string();
        let at = location["range"]["start"]["line"].as_u64()? as u32;
        Some((target, at))
    }

    /// Every reference to what is at a position, as `(uri, line)` in the
    /// order the server sorted them — re-asked until the answer is not empty,
    /// for the reason every other position request here is.
    pub fn references_at(
        &mut self,
        uri: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Vec<(String, u32)> {
        assert!(
            !self.capabilities["referencesProvider"].is_null(),
            "references are not advertised, so no client asks for them"
        );
        let params = serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
            "context": { "includeDeclaration": include_declaration }
        });
        let answer = self.settle("textDocument/references", &params);
        answer["result"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|location| {
                Some((
                    location["uri"].as_str()?.to_string(),
                    location["range"]["start"]["line"].as_u64()? as u32,
                ))
            })
            .collect()
    }

    /// The references at a position, re-asked until `settled` holds of them —
    /// for an answer that improves in a second publish, which the non-empty
    /// rule `references_at` settles on would stop short of.
    pub fn references_until(
        &mut self,
        uri: &str,
        line: u32,
        character: u32,
        settled: impl Fn(&[(String, u32)]) -> bool,
    ) -> Vec<(String, u32)> {
        let params = serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
            "context": { "includeDeclaration": true }
        });
        let found = |answer: &serde_json::Value| -> Vec<(String, u32)> {
            answer["result"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|location| {
                    Some((
                        location["uri"].as_str()?.to_string(),
                        location["range"]["start"]["line"].as_u64()? as u32,
                    ))
                })
                .collect()
        };
        let answer = self
            .server
            .eventually("textDocument/references", &params, |answer| {
                settled(&found(answer))
            });
        found(&answer)
    }

    pub fn hover_at(&mut self, uri: &str, line: u32, character: u32) -> Option<String> {
        assert!(
            !self.capabilities["hoverProvider"].is_null(),
            "hover is not advertised, so no client asks for it"
        );
        let answer = self.settled("textDocument/hover", uri, line, character);
        let contents = &answer["result"]["contents"];
        contents["value"]
            .as_str()
            .or_else(|| contents.as_str())
            .map(str::to_string)
    }

    /// Close a document, as an editor does when its tab is closed.
    pub fn close(&mut self, uri: &str) {
        self.documents.remove(uri);
        self.server.send(&serde_json::json!({
            "jsonrpc": "2.0", "method": "textDocument/didClose",
            "params": { "textDocument": { "uri": uri } }
        }));
    }

    /// Report files changed on disk, as an editor's file watcher does:
    /// `kind` is LSP's `1` created, `2` changed, `3` deleted.
    pub fn files_changed(&mut self, changes: &[(&Path, u32)]) {
        let changes: Vec<serde_json::Value> = changes
            .iter()
            .map(|(path, kind)| serde_json::json!({ "uri": file_uri(path), "type": kind }))
            .collect();
        self.server.send(&serde_json::json!({
            "jsonrpc": "2.0", "method": "workspace/didChangeWatchedFiles",
            "params": { "changes": changes }
        }));
    }

    /// The symbols for `query`, re-asked until `settled` holds of them — for
    /// a check that waits on the index *losing* a symbol, which the
    /// non-empty rule `workspace_symbols` settles on cannot express.
    pub fn workspace_symbols_until(
        &mut self,
        query: &str,
        settled: impl Fn(&[serde_json::Value]) -> bool,
    ) -> serde_json::Value {
        self.server.eventually(
            "workspace/symbol",
            &serde_json::json!({ "query": query }),
            |answer| {
                answer["result"]
                    .as_array()
                    .is_some_and(|symbols| settled(symbols))
            },
        )["result"]
            .clone()
    }

    /// The symbols the workspace index offers for `query`, re-asked until the
    /// background walk has landed one — which is what `settle` already does
    /// for every other answer that arrives later than the request.
    pub fn workspace_symbols(&mut self, query: &str) -> serde_json::Value {
        assert!(
            !self.capabilities["workspaceSymbolProvider"].is_null(),
            "workspace symbols are not advertised, so no client asks for them"
        );
        self.settle("workspace/symbol", &serde_json::json!({ "query": query }))["result"].clone()
    }

    pub fn outline(&mut self, uri: &str) -> serde_json::Value {
        assert!(
            !self.capabilities["documentSymbolProvider"].is_null(),
            "the outline is not advertised, so no client asks for it"
        );
        self.settle(
            "textDocument/documentSymbol",
            &serde_json::json!({ "textDocument": { "uri": uri } }),
        )["result"]
            .clone()
    }

    pub fn folding_ranges(&mut self, uri: &str) -> serde_json::Value {
        assert!(
            !self.capabilities["foldingRangeProvider"].is_null(),
            "folding ranges are not advertised, so no client asks for them"
        );
        self.settle(
            "textDocument/foldingRange",
            &serde_json::json!({ "textDocument": { "uri": uri } }),
        )["result"]
            .clone()
    }

    pub fn selection_ranges(&mut self, uri: &str, positions: Vec<(u32, u32)>) -> serde_json::Value {
        assert!(
            !self.capabilities["selectionRangeProvider"].is_null(),
            "selection ranges are not advertised, so no client asks for them"
        );
        let positions: Vec<_> = positions
            .into_iter()
            .map(|(line, character)| serde_json::json!({ "line": line, "character": character }))
            .collect();
        self.settle(
            "textDocument/selectionRange",
            &serde_json::json!({
                "textDocument": { "uri": uri },
                "positions": positions
            }),
        )["result"]
            .clone()
    }

    pub fn semantic_tokens_full(&mut self, uri: &str) -> serde_json::Value {
        assert!(
            !self.capabilities["semanticTokensProvider"].is_null(),
            "semantic tokens are not advertised, so no client asks for them"
        );
        self.settle(
            "textDocument/semanticTokens/full",
            &serde_json::json!({ "textDocument": { "uri": uri } }),
        )["result"]
            .clone()
    }

    /// The legend the handshake advertised for semantic tokens, so a test can
    /// decode a token's `token_type` index back into the name the server
    /// meant, without hard-coding a second copy of the legend order.
    pub fn semantic_tokens_legend(&self) -> Vec<String> {
        self.capabilities["semanticTokensProvider"]["legend"]["tokenTypes"]
            .as_array()
            .expect("the legend is advertised whenever the capability is")
            .iter()
            .map(|value| value.as_str().expect("a token type name").to_owned())
            .collect()
    }

    /// Every occurrence, in this file, of the binding at `line`/`character`.
    pub fn document_highlights(
        &mut self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> serde_json::Value {
        assert!(
            !self.capabilities["documentHighlightProvider"].is_null(),
            "document highlight is not advertised, so no client asks for it"
        );
        self.settled("textDocument/documentHighlight", uri, line, character)["result"].clone()
    }

    /// A positional request, put through `settle` — so it is re-asked until the
    /// answer stops being empty, or until the deadline.
    ///
    /// Nothing here checks *which* version answered. A non-empty answer from the
    /// pre-edit view satisfies it, so a check that types before it asks must
    /// wait on the reparse itself; `scratch` in `editor_pass.rs` is that wait.
    fn settled(&mut self, method: &str, uri: &str, line: u32, character: u32) -> serde_json::Value {
        let params = serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        });
        self.settle(method, &params)
    }

    /// Re-ask until the answer stops being empty, then return it.
    ///
    /// Reparsing runs on a worker pool, so the request straight after an edit is
    /// answered from the previous view rather than made to wait. The first
    /// answer that is not empty is the one that caught up; a check that expects
    /// an empty answer gets it from the deadline instead, which is why this
    /// returns the last answer rather than failing — a document that genuinely
    /// has no outline, no completions or no definition is a thing tests assert
    /// about, not a server that never caught up.
    //
    // ponytail: a check expecting an empty answer waits out the deadline, so the
    // refusal checks are the slow ones. The upgrade is to wait on the
    // `reparsed document` log line for the version just sent instead of polling;
    // worth doing if the suite's wall-clock becomes a problem.
    fn settle(&mut self, method: &str, params: &serde_json::Value) -> serde_json::Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let last = self.server.eventually(method, params, |_| true);
            // Completion and hover answer an object, never a bare array, so a
            // check for an empty *array* alone is satisfied by the first answer
            // and returns the pre-edit view.
            let items = last["result"]
                .as_array()
                .or_else(|| last["result"]["items"].as_array());
            let empty = last["result"].is_null() || items.is_some_and(|items| items.is_empty());
            if !empty || std::time::Instant::now() >= deadline {
                return last;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The latest `publishDiagnostics` for `uri` once one satisfies
    /// `settled`, asking a cheap question meanwhile so the stream is read.
    pub fn diagnostics_until(
        &mut self,
        uri: &str,
        settled: impl Fn(&[serde_json::Value]) -> bool,
    ) -> Vec<serde_json::Value> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let latest = self
                .server
                .notifications()
                .iter()
                .rev()
                .find(|message| {
                    message["method"] == "textDocument/publishDiagnostics"
                        && message["params"]["uri"] == uri
                })
                .and_then(|message| message["params"]["diagnostics"].as_array().cloned());
            if let Some(diagnostics) = latest
                && settled(&diagnostics)
            {
                return diagnostics;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no settled diagnostics for {uri}; notifications: {:?}",
                self.server.notifications()
            );
            std::thread::sleep(Duration::from_millis(20));
            self.server.eventually(
                "textDocument/documentSymbol",
                &serde_json::json!({ "textDocument": { "uri": uri } }),
                |_| true,
            );
        }
    }

    pub fn shutdown(mut self) -> Option<i32> {
        self.server.send(&serde_json::json!({
            "jsonrpc": "2.0", "id": 9_000, "method": "shutdown", "params": null
        }));
        let _ = self.server.recv();
        self.server.send(&serde_json::json!({
            "jsonrpc": "2.0", "method": "exit", "params": null
        }));
        self.server.wait_for_exit(Duration::from_secs(5))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::no_classpath;

    // The two shapes `format!("file://{}", path.display())` got wrong. Neither
    // is reachable from this machine — the first needs Windows, the second a
    // checkout path nobody has — so they are checked directly or not at all.

    #[test]
    fn a_canonicalised_windows_path_becomes_a_uri_a_client_can_parse() {
        let uri = file_uri(Path::new(r"\\?\C:\src\javelin\Greeter.java"));
        assert_eq!(uri, "file:///C:/src/javelin/Greeter.java");
    }

    #[test]
    fn a_path_a_uri_cannot_hold_verbatim_is_encoded_rather_than_sent_raw() {
        let uri = file_uri(Path::new("/home/a b/c#d/e%f/G.java"));
        assert_eq!(uri, "file:///home/a%20b/c%23d/e%25f/G.java");
    }

    #[test]
    fn the_editor_mirrors_what_it_typed() {
        let mut editor = Editor::launch(&no_classpath());
        let uri = editor.open(&demo_project().join("src/main/java/com/example/Greeter.java"));
        let (line, character) = editor.find(
            &uri,
            "var greeter = new Greeter(DEFAULT, Map.of(DEFAULT, 1));",
        );
        editor.type_at(&uri, line, character, "\n        greeter.greet();");
        assert!(
            editor.text(&uri).contains("greeter.greet();"),
            "the mirror did not record what was typed"
        );
        assert_eq!(
            editor.version(&uri),
            25,
            "one didChange per character typed"
        );
    }

    #[test]
    fn completion_is_refused_where_the_handshake_does_not_invite_it() {
        let mut editor = Editor::launch(&no_classpath());
        let uri = editor.open(&demo_project().join("src/main/java/com/example/Greeter.java"));
        // Just past the `{` of the class body: neither an identifier character
        // nor an advertised trigger, so no real client would ask here.
        let (line, character) = editor.find(&uri, "public class Greeter {");
        let refusal = editor
            .try_completions_at(&uri, line, character)
            .expect_err("no client asks here");
        assert!(
            refusal.contains("trigger"),
            "the refusal must say why: {refusal}"
        );
    }

    #[test]
    fn completion_after_a_dot_is_permitted_because_a_dot_is_advertised() {
        let mut editor = Editor::launch(&no_classpath());
        let uri = editor.open(&demo_project().join("src/main/java/com/example/Greeter.java"));
        let (line, character) = editor.find(&uri, "return seen.");
        assert!(
            editor.try_completions_at(&uri, line, character).is_ok(),
            "`.` is advertised, so a client asks here"
        );
    }
}
