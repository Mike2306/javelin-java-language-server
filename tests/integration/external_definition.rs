//! A jump out of the open buffer and into the JDK's own sources, over a real
//! handshake.
//!
//! The one test in this directory that lets the background classpath read run.
//! It runs against a fabricated JDK — a home whose whole content is a one-entry
//! `lib/src.zip` — rather than the machine's, so what it asserts does not
//! depend on which JDK is installed and the read costs milliseconds instead of
//! eleven seconds. Everything between the client and that archive is real: the
//! reader, the fact cache's schema, the member closure, the extraction and the
//! `file:` URI the client is finally given.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::{env, fs};

use javelin_integration_tests::Client;
use lsp_types::request::Request as _;
use serde_json::{Value, json};

const URI: &str = "file:///A.java";
const BUFFER: &str = "class A {\n  void m(String s) { s.length(); }\n}\n";

/// The archive's only entry. Not the real `String.java`: what the jump has to
/// land on is the offsets the facts carry, and a four-line file makes the
/// expected line and column readable in the assertion.
const ENTRY: &str = "java.base/java/lang/String.java";
const ENTRY_SOURCE: &str =
    "package java.lang;\npublic class String {\n    public int length() { return 0; }\n}\n";

fn scratch(name: &str) -> PathBuf {
    let dir = env::temp_dir()
        .join("javelin-integration-tests")
        .join("external-definition")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// A JDK home as `javelin_classpath::discover` recognises one: a directory
/// with a `lib/src.zip` in it. No `release` file, so the version reads as
/// unknown, which is a shape the real code already has to handle.
fn fake_jdk(dir: &Path) -> PathBuf {
    let home = dir.join("jdk");
    let lib = home.join("lib");
    fs::create_dir_all(&lib).expect("the lib directory");
    let file = fs::File::create(lib.join("src.zip")).expect("src.zip");
    let mut writer = zip::ZipWriter::new(file);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    writer.start_file(ENTRY, options).expect("start the entry");
    writer
        .write_all(ENTRY_SOURCE.as_bytes())
        .expect("write the entry");
    writer.finish().expect("finish the archive");
    home
}

/// The path a `file:` URI names: the percent escapes the server put in, and on
/// Windows the slash it put in front of the drive letter. Decoded rather than
/// assumed plain — a temp directory under a profile with a space in its name is
/// an ordinary Windows machine, and leaving the `%20` in would fail the read
/// below over the path rather than over the jump this test is about.
fn path_of(uri: &str) -> PathBuf {
    let encoded = uri.strip_prefix("file://").expect("a file URI");
    let mut bytes = encoded.bytes();
    let mut decoded = Vec::with_capacity(encoded.len());
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let hex: String = bytes.by_ref().take(2).map(char::from).collect();
            decoded.push(u8::from_str_radix(&hex, 16).expect("a percent escape"));
        } else {
            decoded.push(byte);
        }
    }
    let path = String::from_utf8(decoded).expect("the path was UTF-8 going in");
    let drive = path
        .strip_prefix('/')
        .filter(|rest| rest.as_bytes().get(1) == Some(&b':'));
    PathBuf::from(drive.unwrap_or(&path))
}

#[test]
fn a_definition_on_a_jdk_member_lands_in_the_extracted_source_entry() {
    let dir = scratch("string_length");
    let home = fake_jdk(&dir);
    // `persist` off so this test neither reads nor writes a fact cache another
    // run could have left behind; the extraction it does need is written
    // regardless, since without it there is no file to open.
    let client = Client::with_options(json!({
        "jdk": { "home": home },
        "index": { "cacheDir": dir.join("cache"), "persist": false },
    }));
    client.did_open(URI, BUFFER);

    // The read runs on a thread of its own and nothing waits for it, so the
    // first asks answer null — truthfully, as Phase 2 did. What is promised is
    // that it catches up.
    let location = client.eventually(
        lsp_types::request::GotoDefinition::METHOD,
        json!({
            "textDocument": { "uri": URI },
            "position": { "line": 1, "character": 24 },
        }),
        |answer: &Value| !answer.is_null(),
    );

    let uri = location["uri"].as_str().expect("a URI");
    assert!(
        uri.ends_with(ENTRY),
        "the extracted entry, not a buffer: {uri}"
    );
    assert_ne!(uri, URI, "not the document that asked");
    // `public int length()` on the entry's third line, and the name alone.
    assert_eq!(
        location["range"],
        json!({
            "start": { "line": 2, "character": 15 },
            "end": { "line": 2, "character": 21 },
        })
    );
    // And there is a file there to open.
    assert_eq!(
        fs::read_to_string(path_of(uri)).expect("the extracted file"),
        ENTRY_SOURCE
    );

    client.shutdown();
}
