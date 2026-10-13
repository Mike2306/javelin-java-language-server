//! Reading a dependency JAR's class files into facts, against reading the same
//! facts back out of the per-JAR store — the pair the store exists for.
//!
//! Off the request path: both run on the classpath thread after the JDK's
//! world is published, so no budget applies. Recorded because a project's
//! JARs are read at every start, cached or not, and a Spring application has
//! hundreds of them.
//!
//! The JAR is [`ENTRIES`] copies of the committed `Box.class` fixture, each
//! renamed into a package of its own by rewriting the twelve bytes of
//! `com/acme/Box` wherever they occur — a same-length rewrite, so every
//! length prefix in the constant pool stays true.

use std::io::Write as _;
use std::path::PathBuf;

use criterion::{Criterion, criterion_group, criterion_main};
use javelin_core::CancellationToken;
use javelin_index::CacheDb;

/// A large library's worth of classes — Guava holds about 1 350.
const ENTRIES: usize = 2_000;

fn jar(dir: &std::path::Path, entries: usize) -> PathBuf {
    let class = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tests/fixtures/classfiles/classes/com/acme/Box.class"),
    )
    .expect("the fixture is checked in");
    let path = dir.join("generated.jar");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).expect("jar"));
    let from = b"com/acme/Box";
    for n in 0..entries {
        let to = format!("p{n:04}/ac/Box");
        assert_eq!(to.len(), from.len());
        let mut renamed = class.clone();
        let mut at = 0;
        while let Some(found) = renamed[at..].windows(from.len()).position(|w| w == from) {
            let start = at + found;
            renamed[start..start + from.len()].copy_from_slice(to.as_bytes());
            at = start + from.len();
        }
        zip.start_file(
            format!("p{n:04}/ac/Box.class"),
            zip::write::SimpleFileOptions::default(),
        )
        .expect("entry");
        zip.write_all(&renamed).expect("write");
    }
    zip.finish().expect("finish");
    path
}

fn dependencies(c: &mut Criterion) {
    let benching = std::env::args().any(|argument| argument == "--bench");
    let entries = if benching { ENTRIES } else { 4 };
    let dir = std::env::temp_dir().join(format!("javelin-bench-jar-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    let jar = jar(&dir, entries);
    let cancel = CancellationToken::new();
    let read = || javelin_classpath::read_jar_facts(&jar, 1 << 20, &cancel).expect("a jar");
    let facts = read();
    assert_eq!(facts.len(), entries, "every entry is a class");

    let mut store = CacheDb::open(&dir.join("store.sqlite")).expect("store");
    store
        .write_types("generated.jar", "1-1", &facts)
        .expect("written");
    assert_eq!(store.read_types().expect("read").len(), entries);

    let mut group = c.benchmark_group("dependency_jar");
    group.sample_size(10);
    group.bench_function("read", |b| b.iter(|| std::hint::black_box(read().len())));
    group.bench_function("cached", |b| {
        b.iter(|| std::hint::black_box(store.read_types().expect("read").len()))
    });
    group.finish();
    let _ = std::fs::remove_dir_all(&dir);
}

criterion_group!(benches, dependencies);
criterion_main!(benches);
