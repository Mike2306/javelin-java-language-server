//! Indexing the workspace's own sources, cold against warm — and querying
//! the result.
//!
//! The difference between the two rows is the whole reason the `source_files`
//! table exists. `cold` is a session that has nothing stored: every file is
//! read, parsed and written down. `warm` is the next one: the same walk finds
//! the same files, every modification time and size matches what is stored,
//! and nothing is opened at all. If those two numbers ever converge, the skip
//! has stopped working and every start pays for a full parse of the project.
//!
//! Neither row is on the request path or the startup path — this runs on a
//! detached thread from the `initialized` notification — so there is no budget
//! in `docs/performance.md` for it to meet. It is recorded because a warm
//! start is a product promise measured in hundreds of milliseconds, and a walk
//! that took seconds of a laptop's disk would be felt through the fan even
//! though nothing waits on it.
//!
//! The third group is the reader `workspace/symbol` goes through, which
//! scans every indexed simple name on every keystroke of a symbol picker.
//! That cost is proportional to the workspace rather than to the answer, so
//! it is the one number that says when the linear scan has to become an
//! index — `matching`'s `ponytail:` in `crates/index/src/workspace.rs` names
//! the upgrade. It shares the 50 ms interactive budget every other request
//! has.
//!
//! The tree is generated once and reused by every row: what is measured is
//! indexing a workspace, not creating one. Under `cargo bench` it holds
//! [`FILES`] files, which is a small service; under `cargo test` it shrinks to
//! [`TEST_FILES`], because Criterion runs each closure once there and times
//! nothing, so the count buys only build time.

use std::path::{Path, PathBuf};

use criterion::{Criterion, criterion_group, criterion_main};
use javelin_core::{CancellationToken, Config};
use javelin_index::CacheDb;

/// A small service's worth of source files.
const FILES: usize = 1_000;

/// Enough to exercise the batching boundary (256) and nothing more.
const TEST_FILES: usize = 4;

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("javelin-bench-workspace-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// A workspace of `count` files, laid out in packages the way a real project
/// is: a hundred files per package, so the walk descends rather than reading
/// one enormous directory.
fn workspace(dir: &Path, count: usize) -> PathBuf {
    let root = dir.join("src").join("main").join("java");
    for file in 0..count {
        let package = format!("com.example.p{}", file / 100);
        let path = root.join(package.replace('.', "/"));
        std::fs::create_dir_all(&path).expect("package directory");
        std::fs::write(
            path.join(format!("Type{file}.java")),
            source(&package, file),
        )
        .expect("source file");
    }
    root
}

/// An ordinary domain class: a package, two imports, a supertype, a nested
/// type and a handful of members. Small on purpose — a workspace's cost is its
/// file count far more than any one file's size.
fn source(package: &str, n: usize) -> String {
    format!(
        "package {package};\n\
         \n\
         import java.util.List;\n\
         import java.util.Map;\n\
         \n\
         public class Type{n} extends Base implements java.io.Serializable {{\n\
         \x20   private final List<String> names;\n\
         \x20   private Map<String, Integer> counts;\n\
         \n\
         \x20   public Type{n}(List<String> names) {{\n\
         \x20       this.names = names;\n\
         \x20   }}\n\
         \n\
         \x20   public List<String> names() {{\n\
         \x20       return names;\n\
         \x20   }}\n\
         \n\
         \x20   public interface Visitor {{\n\
         \x20       void visit(Type{n} value);\n\
         \x20   }}\n\
         }}\n"
    )
}

fn index(root: &Path, db: &mut CacheDb) -> usize {
    let cancel = CancellationToken::new();
    let found = javelin_index::java_sources(
        std::slice::from_ref(&root.to_path_buf()),
        Config::default().index.max_file_size_bytes,
        &cancel,
    );
    db.index_sources(&found, &cancel).expect("indexing").len()
}

fn workspace_index(c: &mut Criterion) {
    let benching = std::env::args().any(|argument| argument == "--bench");
    let count = if benching { FILES } else { TEST_FILES };
    let dir = scratch();
    let root = workspace(&dir, count);

    // Two types per file: the class and its nested interface. Asserted rather
    // than assumed, because a walk that silently found nothing would produce a
    // very fast and completely meaningless number.
    let mut warm = CacheDb::in_memory().expect("cache");
    assert_eq!(index(&root, &mut warm), count * 2);

    let mut group = c.benchmark_group("workspace_index");
    group.sample_size(10);
    group.bench_function("cold", |b| {
        b.iter_batched_ref(
            || CacheDb::in_memory().expect("cache"),
            |db| index(&root, db),
            criterion::BatchSize::SmallInput,
        );
    });
    group.bench_function("warm", |b| b.iter(|| index(&root, &mut warm)));
    group.finish();

    // The index the symbol queries below read: the store the rows above
    // measured, read straight out rather than re-walked — `index_sources`
    // with an empty walk would delete every row as gone.
    // The whole index, members included, from the warm store: a warm walk
    // re-reads nothing and hands back everything it holds.
    let cancel = CancellationToken::new();
    let found = javelin_index::java_sources(
        std::slice::from_ref(&root),
        Config::default().index.max_file_size_bytes,
        &cancel,
    );
    let index = warm.index_sources(&found, &cancel).expect("indexing");
    assert_eq!(index.len(), count * 2, "there is nothing to query");
    let mut group = c.benchmark_group("workspace_symbol");
    // A prefix every generated type shares, so the cap is what stops the
    // scan; a substring that is not a prefix, so both tiers are walked; and a
    // query nothing matches, which is the whole scan with nothing to show for
    // it and therefore the worst case.
    for (name, query) in [("prefix", "Type"), ("substring", "isit"), ("miss", "zzzz")] {
        group.bench_function(name, |b| {
            b.iter(|| std::hint::black_box(index.matching(query, 500).len()))
        });
    }
    // Members outnumber types, and a symbol search scans them too: every
    // generated type declares a constructor-free handful of fields and
    // methods plus its nested interface's method.
    for (name, query) in [("members_prefix", "names"), ("members_miss", "zzzz")] {
        group.bench_function(name, |b| {
            b.iter(|| std::hint::black_box(index.matching_members(query, 500).len()))
        });
    }
    group.finish();

    let _ = std::fs::remove_dir_all(&dir);
}

/// The member closure a `.` on a workspace receiver asks for: a type of
/// [`GOD_MEMBERS`] fields, under a chain of [`CHAIN`] workspace supertypes of
/// ten members each, every member's written type resolved against its own
/// file as the walk visits it. It is the new cost on the request path the
/// workspace-members slice added, and it shares the 50 ms budget.
const GOD_MEMBERS: usize = 2_000;
const CHAIN: usize = 20;

fn workspace_members(c: &mut Criterion) {
    use javelin_semantic::Externals as _;
    let dir = std::env::temp_dir().join(format!(
        "javelin-bench-workspace-members-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let package = dir.join("com/example");
    std::fs::create_dir_all(&package).expect("package directory");
    for level in 0..CHAIN {
        let parent = if level == 0 {
            String::new()
        } else {
            format!(" extends Level{}", level - 1)
        };
        let members: String = (0..10)
            .map(|n| {
                format!(
                    "    java.util.List<String> m{level}x{n}(String s, int i) {{ return null; }}\n"
                )
            })
            .collect();
        std::fs::write(
            package.join(format!("Level{level}.java")),
            format!("package com.example;\npublic class Level{level}{parent} {{\n{members}}}\n"),
        )
        .expect("write");
    }
    let fields: String = (0..GOD_MEMBERS)
        .map(|n| format!("    String field{n};\n"))
        .collect();
    std::fs::write(
        package.join("God.java"),
        format!(
            "package com.example;\npublic class God extends Level{} {{\n{fields}}}\n",
            CHAIN - 1
        ),
    )
    .expect("write");
    let cancel = CancellationToken::new();
    let found = javelin_index::java_sources(std::slice::from_ref(&dir), u64::MAX, &cancel);
    let index = CacheDb::in_memory()
        .expect("cache")
        .index_sources(&found, &cancel)
        .expect("indexing");
    let externals = javelin_server::classpath::ClasspathExternals::new(
        std::sync::Arc::new(javelin_classpath::TypeWorld::empty()),
        std::sync::Arc::new(index),
    );
    let expected = GOD_MEMBERS + CHAIN * 10;
    assert_eq!(
        externals
            .members("com.example.God")
            .expect("held")
            .members
            .len(),
        expected,
        "every member of the type and its chain"
    );

    let mut group = c.benchmark_group("workspace_members");
    group.bench_function("god_with_chain", |b| {
        b.iter(|| std::hint::black_box(externals.members("com.example.God")))
    });
    group.finish();

    if std::env::args().any(|argument| argument == "--bench") {
        let mut samples: Vec<std::time::Duration> = (0..100)
            .map(|_| {
                let started = std::time::Instant::now();
                std::hint::black_box(externals.members("com.example.God"));
                started.elapsed()
            })
            .collect();
        if let Some(stats) = javelin_benchmarks::percentiles(&mut samples) {
            println!(
                "workspace_members god_with_chain ({expected} members): p50={:?} p95={:?} max={:?} (budget: p95 < 50ms)",
                stats.p50, stats.p95, stats.max
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The member-resolution pass cross-file member references are read out of:
/// every selected member of every file resolved with definition's own query,
/// on the workspace worker after the walk and again for each changed file and
/// its dependents. Off the request path, so no budget — recorded because a
/// first session pays it once over the whole tree, and a type every file
/// depends on pays it again over every file on each edit to it.
///
/// Each generated file holds a peer of the next file's type, in the next
/// package, and selects four of its members, so every selection crosses a
/// file. The text is read before timing: what is measured is resolving, not
/// the disk.
fn member_resolution(c: &mut Criterion) {
    let benching = std::env::args().any(|argument| argument == "--bench");
    let count = if benching { FILES } else { TEST_FILES };
    let dir =
        std::env::temp_dir().join(format!("javelin-bench-member-refs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let fqn = |n: usize| format!("com.example.p{}.Type{n}", n / 100);
    let mut files = Vec::with_capacity(count);
    for n in 0..count {
        let package = format!("com.example.p{}", n / 100);
        let path = dir.join(package.replace('.', "/"));
        std::fs::create_dir_all(&path).expect("package directory");
        let path = path.join(format!("Type{n}.java"));
        let text = format!(
            "package {package};\n\
             \n\
             public class Type{n} {{\n\
             \x20   public {peer} peer;\n\
             \n\
             \x20   public int size() {{\n\
             \x20       return 0;\n\
             \x20   }}\n\
             \n\
             \x20   public int chain() {{\n\
             \x20       return peer.size() + peer.peer.size() + peer.chain();\n\
             \x20   }}\n\
             }}\n",
            peer = fqn((n + 1) % count)
        );
        std::fs::write(&path, &text).expect("source file");
        files.push((path, text));
    }
    let cancel = CancellationToken::new();
    let found = javelin_index::java_sources(std::slice::from_ref(&dir), u64::MAX, &cancel);
    let index = CacheDb::in_memory()
        .expect("cache")
        .index_sources(&found, &cancel)
        .expect("indexing");
    let externals = javelin_server::classpath::ClasspathExternals::new(
        std::sync::Arc::new(javelin_classpath::TypeWorld::empty()),
        std::sync::Arc::new(index),
    );
    let mut parser = javelin_parser::JavaParser::new().expect("parser");
    let resolve_all = |parser: &mut javelin_parser::JavaParser| -> usize {
        files
            .iter()
            .map(|(path, text)| {
                javelin_server::member_refs::resolve_file(
                    path,
                    text.clone(),
                    parser,
                    &externals,
                    &cancel,
                )
                .expect("not cancelled")
                .map_or(0, |resolved| resolved.refs.len())
            })
            .sum()
    };
    // `peer.size()`, `peer.peer`, the `size()` after it and `peer.chain()`:
    // asserted, because a pass that resolved nothing would be very fast.
    assert_eq!(
        resolve_all(&mut parser),
        count * 4,
        "every selection crosses a file"
    );

    let mut group = c.benchmark_group("member_resolution");
    group.sample_size(10);
    group.bench_function("whole_tree", |b| {
        b.iter(|| std::hint::black_box(resolve_all(&mut parser)))
    });
    group.finish();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `textDocument/implementation` on an interface [`FILES`] workspace types
/// implement, and on its method, which each overrides — the request-path walk
/// the inverse edges add, against the 50 ms budget. Every candidate's written
/// supertypes are resolved against its own file per query.
fn hierarchy(c: &mut Criterion) {
    use javelin_semantic::Externals as _;
    let benching = std::env::args().any(|argument| argument == "--bench");
    let count = if benching { FILES } else { TEST_FILES };
    let dir = std::env::temp_dir().join(format!("javelin-bench-hierarchy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let base = dir.join("p");
    std::fs::create_dir_all(&base).expect("package");
    std::fs::write(
        base.join("Shape.java"),
        "package p;\npublic interface Shape { double area(); }\n",
    )
    .expect("Shape");
    for n in 0..count {
        let package = dir.join(format!("q{}", n / 100));
        std::fs::create_dir_all(&package).expect("package");
        std::fs::write(
            package.join(format!("Impl{n}.java")),
            format!(
                "package q{};\nimport p.Shape;\npublic class Impl{n} implements Shape {{\n    \
                 public double area() {{ return {n}; }}\n}}\n",
                n / 100
            ),
        )
        .expect("Impl");
    }
    let cancel = CancellationToken::new();
    let found = javelin_index::java_sources(std::slice::from_ref(&dir), u64::MAX, &cancel);
    let index = std::sync::Arc::new(
        CacheDb::in_memory()
            .expect("cache")
            .index_sources(&found, &cancel)
            .expect("indexing"),
    );
    let externals = javelin_server::classpath::ClasspathExternals::new(
        std::sync::Arc::new(javelin_classpath::TypeWorld::empty()),
        std::sync::Arc::clone(&index),
    );
    let subtypes = || javelin_server::hierarchy::all_subtypes(&externals, &index, "p.Shape").len();
    assert_eq!(subtypes(), count, "every Impl implements Shape");
    let start = index.members_of("p.Shape")[0].name_start;
    let area = javelin_server::hierarchy::member(&externals, "p.Shape", start).expect("area");
    assert!(externals.members("p.Shape").is_some());
    let overrides =
        || javelin_server::hierarchy::overrides(&externals, &index, "p.Shape", &area).len();
    assert_eq!(overrides(), count, "every Impl overrides area");

    let mut group = c.benchmark_group("hierarchy");
    group.bench_function("implementations_of_a_type", |b| {
        b.iter(|| std::hint::black_box(subtypes()))
    });
    group.bench_function("overrides_of_a_method", |b| {
        b.iter(|| std::hint::black_box(overrides()))
    });
    group.finish();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The worker's pass over one god-class that extends a workspace type and
/// names two of its fields bare in every method, beside two type names: each
/// bare name asks what the types around it hold as fields, which must be
/// read once per file and not once per name.
fn bare_names_in_a_subclass(c: &mut Criterion) {
    let benching = std::env::args().any(|argument| argument == "--bench");
    let methods = if benching { GOD_MEMBERS * 2 } else { 50 };
    let dir = std::env::temp_dir().join(format!("javelin-bench-bare-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let package = dir.join("p");
    std::fs::create_dir_all(&package).expect("package");
    std::fs::write(
        package.join("Base.java"),
        "package p;\n\npublic class Base {\n    protected int count;\n    protected static final int K = 1;\n}\n",
    )
    .expect("base");
    let mut text = String::from("package p;\n\npublic class Caller extends Base {\n");
    for n in 0..methods {
        text.push_str(&format!(
            "    int f{n};\n    int m{n}() {{ return Math.abs(K) + Integer.MAX_VALUE + count + f{n}; }}\n"
        ));
    }
    text.push_str("}\n");
    let path = package.join("Caller.java");
    std::fs::write(&path, &text).expect("caller");
    let cancel = CancellationToken::new();
    let found = javelin_index::java_sources(std::slice::from_ref(&dir), u64::MAX, &cancel);
    let index = CacheDb::in_memory()
        .expect("cache")
        .index_sources(&found, &cancel)
        .expect("indexing");
    let externals = javelin_server::classpath::ClasspathExternals::new(
        std::sync::Arc::new(javelin_classpath::TypeWorld::empty()),
        std::sync::Arc::new(index),
    );
    let mut parser = javelin_parser::JavaParser::new().expect("parser");
    let mut resolve = || {
        javelin_server::member_refs::resolve_file(
            &path,
            text.clone(),
            &mut parser,
            &externals,
            &cancel,
        )
        .expect("not cancelled")
        .map_or(0, |resolved| resolved.refs.len())
    };
    assert_eq!(
        resolve(),
        methods * 2,
        "every bare `K` and `count` is Base's"
    );
    let mut group = c.benchmark_group("member_resolution");
    group.sample_size(10);
    group.bench_function("bare_names_in_a_subclass", |b| {
        b.iter(|| std::hint::black_box(resolve()))
    });
    group.finish();
    let _ = std::fs::remove_dir_all(&dir);
}

criterion_group!(
    benches,
    workspace_index,
    workspace_members,
    member_resolution,
    bare_names_in_a_subclass,
    hierarchy
);
criterion_main!(benches);
