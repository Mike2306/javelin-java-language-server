//! Member completion for a browser worker: a WASI *reactor* (no `_start`), loaded once with the
//! facts of a JDK and then asked about one buffer at a time.
//!
//! No server, no threads, no files: `javelin-server`'s loop needs threads and `wasm32-wasip1` has
//! none, so this calls the engine the way the completion handler does, with `TypeWorld` built from
//! facts shipped as JSON instead of read out of `src.zip`.
//!
//! ABI, all pointers into this module's linear memory:
//!
//! - `hx_alloc(len) -> ptr`, `hx_free(ptr, len)` — buffers the host fills and reads;
//! - `hx_load(ptr, len) -> i32` — the facts, a JSON array of `TypeFacts`; `0` is success;
//! - `hx_complete(ptr, len) -> ptr` — a JSON request `{"text", "line", "character"}`; the answer
//!   is `[u32 LE length][JSON]` at the returned pointer, to be freed with `hx_free(ptr, 4 + length)`.
//!
//! A panic aborts the instance on this target, so the host re-instantiates after a trap.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::sync::Arc;

use javelin_classpath::{TypeFacts, TypeWorld};
use javelin_document::{DocumentId, Position, TextDocument};
use javelin_index::WorkspaceIndex;
use javelin_parser::{DefinitionKind, FileSyntax, JavaParser, ScopeTree};
use javelin_semantic::Files;
use javelin_server::classpath::ClasspathExternals;
use serde::{Deserialize, Serialize};

struct Engine {
    externals: ClasspathExternals,
    parser: JavaParser,
}

thread_local! {
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
}

#[derive(Deserialize)]
struct Request {
    text: String,
    line: u32,
    character: u32,
}

#[derive(Serialize)]
struct Item {
    label: String,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    documentation: Option<String>,
}

#[derive(Serialize)]
struct Answer {
    items: Vec<Item>,
    /// Whether the list is everything the receiver's type exposes under the typed prefix.
    complete: bool,
}

fn kind(kind: DefinitionKind) -> &'static str {
    match kind {
        DefinitionKind::LocalVariable
        | DefinitionKind::Parameter
        | DefinitionKind::PatternBinding => "variable",
        DefinitionKind::Field => "field",
        DefinitionKind::TypeParameter => "typeParameter",
        DefinitionKind::Type => "class",
        DefinitionKind::Method => "method",
        DefinitionKind::Constructor => "constructor",
    }
}

fn load(facts: &[u8]) -> Result<(), String> {
    let facts: Vec<TypeFacts> = serde_json::from_slice(facts).map_err(|err| err.to_string())?;
    let world = Arc::new(TypeWorld::new(facts));
    let externals = ClasspathExternals::new(world, Arc::new(WorkspaceIndex::new(Vec::new())));
    let parser = JavaParser::new().map_err(|err| err.to_string())?;

    ENGINE.with(|engine| *engine.borrow_mut() = Some(Engine { externals, parser }));

    Ok(())
}

fn complete(request: &[u8]) -> Result<Answer, String> {
    let request: Request = serde_json::from_slice(request).map_err(|err| err.to_string())?;

    ENGINE.with(|engine| {
        let mut engine = engine.borrow_mut();
        let engine = engine.as_mut().ok_or("facts are not loaded")?;

        let id = DocumentId::new("file:///Main.java");
        let document = TextDocument::new(id.clone(), 1, request.text);
        let offset = document.offset_at(Position::new(request.line, request.character));
        let tree = engine
            .parser
            .parse(&document)
            .map_err(|err| err.to_string())?;
        let syntax = FileSyntax::build(&document, &tree);
        let scopes = ScopeTree::build(&document, &tree);
        let files = Files::single(id, &syntax, &scopes);

        let found = javelin_semantic::member_completions_at(
            &document,
            &tree,
            &files,
            &engine.externals,
            offset,
        );

        Ok(match found {
            Some(found) => Answer {
                items: found
                    .candidates
                    .into_iter()
                    .map(|candidate| Item {
                        label: candidate.name,
                        kind: kind(candidate.kind),
                        detail: candidate.detail,
                        documentation: candidate.doc,
                    })
                    .collect(),
                complete: found.complete,
            },
            None => Answer {
                items: Vec::new(),
                complete: false,
            },
        })
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn hx_alloc(len: usize) -> *mut u8 {
    Box::into_raw(vec![0u8; len].into_boxed_slice()).cast()
}

/// # Safety
/// `ptr` and `len` come from `hx_alloc(len)` (or the pointer `hx_complete` returned, with its
/// full `4 + length`) and are freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hx_free(ptr: *mut u8, len: usize) {
    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) });
}

/// # Safety
/// `ptr` points at `len` initialised bytes from `hx_alloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hx_load(ptr: *const u8, len: usize) -> i32 {
    match load(unsafe { std::slice::from_raw_parts(ptr, len) }) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// # Safety
/// `ptr` points at `len` initialised bytes from `hx_alloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hx_complete(ptr: *const u8, len: usize) -> *mut u8 {
    let json = match complete(unsafe { std::slice::from_raw_parts(ptr, len) }) {
        Ok(answer) => serde_json::to_vec(&answer).unwrap_or_default(),
        Err(_) => b"{\"items\":[],\"complete\":false}".to_vec(),
    };

    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    Box::into_raw(out.into_boxed_slice()).cast()
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use javelin_classpath::read_type_facts;
    use javelin_core::CancellationToken;
    use zip::write::SimpleFileOptions;

    use super::*;

    /// The facts of a two-class `java.base`, read the way `examples/jdk-facts.rs` reads a real one.
    fn facts() -> Vec<u8> {
        let dir = std::env::temp_dir().join("javelin-wasm-tests");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("src.zip");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&path).expect("zip file"));
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

        for (name, source) in [
            (
                "java.base/java/lang/Object.java",
                "package java.lang;\npublic class Object {\n    public String toString() { return null; }\n}\n",
            ),
            (
                "java.base/java/lang/String.java",
                "package java.lang;\npublic final class String {\n    public int length() { return 0; }\n    public boolean isEmpty() { return true; }\n}\n",
            ),
        ] {
            writer.start_file(name, options).expect("start entry");
            writer.write_all(source.as_bytes()).expect("write entry");
        }
        writer.finish().expect("finish zip");

        let facts = read_type_facts(&path, u64::MAX, &CancellationToken::never()).expect("facts");

        serde_json::to_vec(&facts).expect("json")
    }

    fn ask(text: &str, line: u32, character: u32) -> Answer {
        let request = serde_json::json!({ "text": text, "line": line, "character": character });

        complete(request.to_string().as_bytes()).expect("answer")
    }

    // One test: the engine lives in a thread-local, and tests run in parallel threads.
    #[test]
    fn completes_the_members_of_a_loaded_type() {
        assert!(complete(b"{}").is_err(), "nothing is loaded yet");
        load(&facts()).expect("facts load");

        let text =
            "class A {\n    void run() {\n        String name = \"x\";\n        name.\n    }\n}\n";
        let answer = ask(text, 3, 13);
        let labels: Vec<_> = answer
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect();

        assert!(labels.contains(&"length"), "{labels:?}");
        assert!(labels.contains(&"isEmpty"), "{labels:?}");
        assert!(
            labels.contains(&"toString"),
            "inherited members too: {labels:?}"
        );
        assert_eq!(
            answer
                .items
                .iter()
                .find(|item| item.label == "length")
                .and_then(|item| item.detail.as_deref()),
            Some("int length()")
        );

        // A cursor that is not after a dot offers nothing: the host keeps its own words.
        assert!(ask(text, 3, 8).items.is_empty());
    }

    fn put(bytes: &[u8]) -> *mut u8 {
        let pointer = hx_alloc(bytes.len());

        // SAFETY: `hx_alloc(len)` returned `len` writable bytes
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len()) };

        pointer
    }

    /// What a host does, through the C ABI alone: fill buffers the module allocated, call, read the
    /// length-prefixed answer, free every buffer with the length it was given.
    #[test]
    fn the_c_abi_round_trips_a_buffer() {
        let facts = facts();
        let loaded = put(&facts);

        // SAFETY: `loaded` holds `facts.len()` bytes from `hx_alloc`, freed once
        unsafe {
            assert_eq!(hx_load(loaded, facts.len()), 0);
            hx_free(loaded, facts.len());
        }

        let request = serde_json::json!({
            "text": "class A {\n    void run() {\n        String name = \"x\";\n        name.\n    }\n}\n",
            "line": 3,
            "character": 13,
        })
        .to_string();
        let input = put(request.as_bytes());

        // SAFETY: the answer is `[u32 LE length][JSON]`, `4 + length` bytes from `hx_alloc`'s allocator
        let answer = unsafe {
            let output = hx_complete(input, request.len());
            hx_free(input, request.len());

            let length =
                u32::from_le_bytes(std::slice::from_raw_parts(output, 4).try_into().unwrap())
                    as usize;
            let json = std::slice::from_raw_parts(output.add(4), length).to_vec();
            hx_free(output, 4 + length);

            json
        };
        let answer: serde_json::Value =
            serde_json::from_slice(&answer).expect("the answer is JSON");
        let labels: Vec<_> = answer["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|item| item["label"].as_str().expect("label"))
            .collect();

        assert!(labels.contains(&"length"), "{labels:?}");
        assert_eq!(answer["complete"], true);
    }
}
