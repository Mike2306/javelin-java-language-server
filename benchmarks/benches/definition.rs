//! Definition latency, across the three shapes an open buffer answers it for:
//! a local, a field, and an unqualified call.
//!
//! Never benchmarked before Task 9 — `docs/performance.md` records a 50 ms p95
//! budget and says the feature is "neither met nor missed" for want of a
//! number. `local` and `field` land in `javelin_parser::definition_at`, the
//! file-local query Phase 1 shipped; `call` is `javelin_semantic::
//! call_definition_at`, added in 2b/2c, on the case it was built for — exactly
//! one method of that name fits the call, so `sole_candidate` finds its answer
//! without asking `apply.rs` to eliminate anything. The ambiguous case, where
//! elimination runs and does not exit early, is `overload_elimination`, not
//! this file.
//!
//! Warm only, no `after_edit`: `completion.rs` already reports what a keystroke
//! costs by way of the reparse and the view rebuild it forces, and repeating
//! that measurement here would only restate it under a different name.
//!
//! The `definition_external` group is the fourth shape, new in Phase 3b's
//! definition slice: the cursor is on a member of a classpath type, so the
//! answer is a location in the JDK's own sources and the server has to put
//! that file on disk before a client can open it. Two rows, because the two
//! costs are different in kind: `cold` opens the archive, decompresses the
//! entry, writes it and converts the member's byte offsets to a UTF-16 range,
//! and `warm` reads the file this session already wrote and converts.
//!
//! It does not go through `javelin_server::handlers::definition`, for the
//! reason `completion.rs`'s `member_external_warm` does not go through its own
//! handler: `ServerState`'s classpath is installed by the background job and
//! nothing public injects one. `JdkSources` is crate-private on top of that,
//! so what runs below is the same sequence `crates/server/src/jdk_sources.rs`
//! runs, written out here — archive open, capped read, temporary-file write
//! and rename, `TextDocument::position_at`. **It pays no handler, no member
//! resolution and no JSON, so its number is not comparable with the three
//! rows above it**; it is the file work a jump into the JDK adds to whatever
//! those already cost.
//!
//! The archive is synthetic and a real JDK is deliberately not read: which one
//! is installed is a property of the machine, and reading one costs eleven
//! seconds. What is copied from a real one is the number that the cold row
//! turns on — a `src.zip`'s central directory is parsed whole on open, so
//! under `cargo bench` the fixture carries [`ARCHIVE_ENTRIES`] of them, the
//! count
//! `docs/performance.md` records for the Temurin 25 every other JDK figure in
//! this repository came from. The entries around the target are stubs, so the
//! fixture is a few MB rather than 53, and the archive's *size* is not what
//! opening it reads. Measured once to check that claim and not committed: the
//! fixture opens in 23.9 ms and the machine's real `src.zip` in 23.6 ms, so
//! the entry count is the whole of it — and the open is ~99% of the cold row.
//! Under `cargo test` the fixture shrinks to [`TEST_ENTRIES`]: Criterion runs
//! each closure once there and times nothing, so the count buys only build
//! time.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use javelin_benchmarks::{Tier, java_source, percentiles};
use javelin_classpath::SourceArchive;
use javelin_core::{CancellationToken, Config};
use javelin_document::{DocumentId, Position, Range, TextDocument};
use javelin_server::ServerState;
use lsp_types::{GotoDefinitionParams, GotoDefinitionResponse};

const URI: &str = "bench://Generated.java";

fn opened(tier: Tier) -> ServerState {
    let mut state = ServerState::new(Config::default()).expect("engine");
    state.did_open(DocumentId::new(URI), 1, java_source(tier));
    state
}

/// The position `skip` bytes past the first occurrence of `marker` at or after
/// the middle of the file — the same halfway-down-the-file convention
/// `completion.rs`'s `cursor` uses, so these positions grow with the tier the
/// same way its does.
fn position_after(document: &TextDocument, marker: &str, skip: usize) -> Position {
    let text = document.text();
    let middle = text.len() / 2;
    let found = text
        .get(middle..)
        .and_then(|rest| rest.find(marker))
        .map_or(middle, |found| middle + found);
    document.position_at((found + skip) as u32)
}

/// The callee's name in the sibling call `java_source` adds to every generated
/// method, at or after the middle of the file.
///
/// Not `position_after`: the callee's name is `method{n}`, and `n`'s width
/// varies with the tier, so no fixed byte offset past a fixed marker lands on
/// its start. Scanning back from the `(` to the nearest non-identifier
/// character does, whatever `n` is.
fn call_site(document: &TextDocument) -> Position {
    let text = document.text();
    let middle = text.len() / 2;
    let paren = text
        .get(middle..)
        .and_then(|rest| rest.find("(argument);"))
        .map_or(middle, |found| middle + found);
    let name_start = text[..paren]
        .rfind(|c: char| !c.is_ascii_alphanumeric())
        .map_or(0, |at| at + 1);
    document.position_at(name_start as u32)
}

fn params(at: Position) -> GotoDefinitionParams {
    serde_json::from_value(serde_json::json!({
        "textDocument": { "uri": URI },
        "position": { "line": at.line, "character": at.character },
    }))
    .expect("well-formed params")
}

/// One request, JSON included — the client waits for that too.
///
/// Returns the answer so a caller can assert it actually resolved: a fixture
/// or offset regression that silently made a position answer `None` would
/// otherwise still produce a fast, plausible-looking number, which is exactly
/// the failure this task exists to close.
fn define(
    state: &mut ServerState,
    params: &GotoDefinitionParams,
) -> Option<GotoDefinitionResponse> {
    let answer =
        javelin_server::handlers::definition::handle(state, params, &CancellationToken::new())
            .expect("not cancelled");
    let _ = serde_json::to_value(&answer).expect("serialisable");
    answer
}

fn definition(c: &mut Criterion) {
    let mut group = c.benchmark_group("definition");
    for tier in Tier::ALL {
        let source = java_source(tier);
        let document = TextDocument::new(DocumentId::new(URI), 1, source.clone());
        let cases = [
            (
                "local",
                position_after(&document, "if (local > ", "if (".len()),
            ),
            ("field", position_after(&document, "+ field", "+ ".len())),
            ("call", call_site(&document)),
        ];

        for (name, at) in cases {
            let mut state = opened(tier);
            let request = params(at);
            assert!(
                define(&mut state, &request).is_some(),
                "definition {name} {} did not resolve",
                tier.name()
            );
            group.bench_function(BenchmarkId::new(name, tier.name()), |b| {
                b.iter(|| define(&mut state, &request));
            });
        }
    }
    group.finish();

    // Same rule `completion.rs` follows: `cargo bench` passes `--bench` and
    // `cargo test` does not, and the loops below are plain code that would
    // otherwise run unbounded on every CI job.
    if !std::env::args().any(|argument| argument == "--bench") {
        return;
    }

    for tier in Tier::ALL {
        let source = java_source(tier);
        let document = TextDocument::new(DocumentId::new(URI), 1, source.clone());
        let cases = [
            (
                "local",
                position_after(&document, "if (local > ", "if (".len()),
            ),
            ("field", position_after(&document, "+ field", "+ ".len())),
            ("call", call_site(&document)),
        ];

        for (name, at) in cases {
            let mut state = opened(tier);
            let request = params(at);
            assert!(
                define(&mut state, &request).is_some(),
                "definition {name} {} did not resolve",
                tier.name()
            );

            let mut samples: Vec<Duration> = (0..200)
                .map(|_| {
                    let started = Instant::now();
                    define(&mut state, &request);
                    started.elapsed()
                })
                .collect();
            report(tier, name, &mut samples);
        }
    }
}

/// Entries in a real JDK's `src.zip`: Temurin 25's, the archive
/// `docs/performance.md` records at 53 646 286 bytes and 15 371 entries. The
/// count is the fixture's realism, because opening an archive parses its
/// central directory and nothing else.
const ARCHIVE_ENTRIES: usize = 15_371;

/// What the same fixture holds when nobody is measuring it.
///
/// Under `cargo test --all-targets` — three platforms, every CI run — the
/// entry count buys nothing, because Criterion runs each closure once and
/// times nothing, so all that is left is the check that this bench still runs.
/// Writing fifteen thousand entries in a debug build to prove that
/// costs about four seconds; sixty-four proves it just as well.
const TEST_ENTRIES: usize = 64;

/// The entry a jump lands in, named the way a JDK names one: module, then the
/// package as directories.
const TARGET: &str = "java.base/com/example/Generated.java";

/// The file a client is pointed at, under a directory of this bench's own.
/// The server puts it under the cache root keyed by JDK identity; where it
/// goes costs nothing, writing it does.
///
/// Per process, because the directory is wiped on the way in: `cargo bench`
/// and `cargo test --all-targets` can be running at the same time, and one
/// clearing the other's fixture out from under it would fail a run for a
/// reason that has nothing to do with the code.
fn scratch() -> PathBuf {
    let dir = std::env::temp_dir()
        .join("javelin-benchmarks")
        .join(format!("definition-jdk-sources-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// A `src.zip` holding `entries` entries, of which one is `target`.
///
/// The target is deflated, because a JDK's is and decompressing it is part of
/// what the cold row measures. The stubs around it are stored: they exist to
/// make the central directory the size a real one is, and deflating fifteen
/// thousand of them would only slow the fixture down.
fn synthetic_archive(dir: &Path, target: &str, entries: usize) -> PathBuf {
    let path = dir.join("src.zip");
    let file = fs::File::create(&path).expect("zip file");
    let mut writer = zip::ZipWriter::new(file);
    let deflated = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    writer.start_file(TARGET, deflated).expect("start entry");
    writer
        .write_all(target.as_bytes())
        .expect("write the target entry");
    for i in 0..entries - 1 {
        let name = format!("java.base/com/example/filler/Filler{i}.java");
        writer.start_file(name, stored).expect("start entry");
        writer
            .write_all(format!("package com.example.filler;\nclass Filler{i} {{}}\n").as_bytes())
            .expect("write entry");
    }
    writer.finish().expect("finish zip");
    path
}

/// The first jump of a session into one JDK source file: open the archive,
/// read the entry out of it under the same cap the bulk read uses, put it on
/// disk, and turn the member's byte offsets into a UTF-16 range.
fn cold(src_zip: &Path, destination: &Path, name: (u32, u32)) -> Range {
    let mut archive = SourceArchive::open(src_zip).expect("the archive opens");
    let cap = Config::default().index.max_file_size_bytes;
    let text = archive.read(TARGET, cap).expect("the entry reads");
    write_atomically(destination, &text);
    convert(text, name)
}

/// Every jump after that into the same file: the entry is already on disk.
fn warm(destination: &Path, name: (u32, u32)) -> Range {
    let text = fs::read_to_string(destination).expect("the extracted entry");
    convert(text, name)
}

/// Byte offsets to a UTF-16 range, over a document built for this one file —
/// exactly what `handlers::definition` does with the site a fact carries, and
/// the reason the offsets are stored rather than a range.
fn convert(text: String, (start, end): (u32, u32)) -> Range {
    let document = TextDocument::new(DocumentId::new("file:///bench/Generated.java"), 0, text);
    Range::new(document.position_at(start), document.position_at(end))
}

/// Temporary name then rename, as `jdk_sources.rs` writes: a killed process
/// leaves a stray temporary rather than a truncated file a later session
/// believes.
fn write_atomically(destination: &Path, text: &str) {
    let parent = destination.parent().expect("a parent directory");
    fs::create_dir_all(parent).expect("the extraction directory");
    let temporary = parent.join(".Generated.java.tmp");
    fs::write(&temporary, text).expect("write the entry");
    fs::rename(&temporary, destination).expect("rename into place");
}

/// The byte offsets of a member's own name in the generated source — what a
/// `MemberFacts` carries and what the jump lands on.
fn member_name(text: &str) -> (u32, u32) {
    let start = text
        .find("method100(")
        .expect("the fixture declares method100");
    (start as u32, (start + "method100".len()) as u32)
}

fn definition_external(c: &mut Criterion) {
    // Read before the fixture rather than after the group, unlike every other
    // bench here: it decides how large the fixture is, and the size is the
    // whole cost of building one.
    let benching = std::env::args().any(|argument| argument == "--bench");
    let dir = scratch();
    // `Tier::Medium` is ~45 KB, which is a JDK source file of ordinary size:
    // `String.java` is several times larger and the median entry is smaller.
    // The tier is not varied, because what this measures is one file and an
    // archive, not a workspace.
    let entry = java_source(Tier::Medium);
    let src_zip = synthetic_archive(
        &dir,
        &entry,
        if benching {
            ARCHIVE_ENTRIES
        } else {
            TEST_ENTRIES
        },
    );
    let destination = dir.join("extracted").join("Generated.java");
    let name = member_name(&entry);

    // What the warm row reads. Extracted here rather than left to the cold
    // row, so that the two rows do not depend on each other's having run —
    // Criterion takes a filter, and a `warm` run on its own must still work.
    assert_eq!(
        cold(&src_zip, &destination, name),
        warm(&destination, name),
        "the two rows answer the same range, or they are not two measurements \
         of one jump"
    );

    let mut group = c.benchmark_group("definition_external");
    group.bench_function("cold", |b| {
        b.iter(|| cold(&src_zip, &destination, name));
    });
    group.bench_function("warm", |b| {
        b.iter(|| warm(&destination, name));
    });
    group.finish();

    if !benching {
        return;
    }

    let mut samples: Vec<Duration> = (0..200)
        .map(|_| {
            let started = Instant::now();
            cold(&src_zip, &destination, name);
            started.elapsed()
        })
        .collect();
    report_external("cold", &mut samples);

    let mut samples: Vec<Duration> = (0..200)
        .map(|_| {
            let started = Instant::now();
            warm(&destination, name);
            started.elapsed()
        })
        .collect();
    report_external("warm", &mut samples);
}

fn report_external(what: &str, samples: &mut [Duration]) {
    if let Some(stats) = percentiles(samples) {
        println!(
            "definition external {what}: p50={:?} p95={:?} p99={:?} max={:?} \
             (budget: p95 < 50ms)",
            stats.p50, stats.p95, stats.p99, stats.max
        );
    }
}

fn report(tier: Tier, what: &str, samples: &mut [Duration]) {
    if let Some(stats) = percentiles(samples) {
        println!(
            "definition {what} {}: p50={:?} p95={:?} p99={:?} max={:?} (budget: p95 < 50ms)",
            tier.name(),
            stats.p50,
            stats.p95,
            stats.p99,
            stats.max
        );
    }
}

criterion_group!(benches, definition, definition_external);
criterion_main!(benches);
