//! A JDK small enough to read in milliseconds and identical on every machine.
//!
//! The real archive takes about eleven seconds and holds whatever the developer
//! installed, so a check built on it would be slow and would assert something
//! different on every machine. `javelin_core::JdkConfig::home` is settable and
//! `is_jdk_home` accepts any directory with a `release` file or a
//! `lib/src.zip`, so a fixture is enough.
//!
//! The sources below are the types the manual pass names, and nothing else.

use std::io::Write;
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;

/// `(entry name, source)` for every type the checks reach.
///
/// `String`'s *layout* is load-bearing as well as its members:
/// `definition_on_an_external_member_lands_in_the_jdks_own_sources` asserts the
/// line `length()` is declared on, because reaching the file proves less than
/// reaching the member.
const SOURCES: &[(&str, &str)] = &[
    (
        "java/lang/Object.java",
        r#"
package java.lang;
public class Object {
    public String toString() { return null; }
    protected Object clone() { return null; }
    protected void finalize() {}
}
"#,
    ),
    (
        "java/lang/String.java",
        r#"
package java.lang;
public final class String implements java.io.Serializable {
    public int length() { return 0; }
    /** Returns a substring, {@code beginIndex} to {@code endIndex}. */
    public String substring(int beginIndex, int endIndex) { return null; }
    public static String valueOf(int i) { return null; }
}
"#,
    ),
    (
        "java/lang/Integer.java",
        r#"
package java.lang;
public final class Integer implements java.io.Serializable {
    public static int parseInt(String s) { return 0; }
    public int intValue() { return 0; }
}
"#,
    ),
    (
        "java/lang/Long.java",
        "package java.lang;\npublic final class Long {}\n",
    ),
    (
        "java/lang/Record.java",
        "package java.lang;\npublic abstract class Record {}\n",
    ),
    (
        "java/io/Serializable.java",
        "package java.io;\npublic interface Serializable {}\n",
    ),
    (
        "java/util/Map.java",
        r#"
package java.util;
public interface Map<K, V> {
    V get(Object key);
    int size();
    static <K, V> Map<K, V> of(K k, V v) { return null; }
    interface Entry<K, V> { K getKey(); }
}
"#,
    ),
    (
        "java/util/List.java",
        r#"
package java.util;
public interface List<E> extends java.lang.Iterable<E> {
    java.util.Iterator<E> iterator();
    E get(int index);
    int size();
    java.util.stream.Stream<E> stream();
}
"#,
    ),
    (
        "java/lang/Iterable.java",
        r#"
package java.lang;
public interface Iterable<T> {
    java.util.Iterator<T> iterator();
    default void forEach(java.util.function.Consumer<? super T> action) {}
}
"#,
    ),
    (
        "java/util/Iterator.java",
        r#"
package java.util;
public interface Iterator<E> {
    boolean hasNext();
    E next();
}
"#,
    ),
    (
        "java/lang/Class.java",
        r#"
package java.lang;
public final class Class<T> {
    public String getName() { return null; }
    public T cast(Object obj) { return null; }
}
"#,
    ),
    (
        "java/util/function/Consumer.java",
        r#"
package java.util.function;
public interface Consumer<T> {
    void accept(T t);
    default Consumer<T> andThen(Consumer<? super T> after) { return null; }
}
"#,
    ),
    (
        "java/util/function/Predicate.java",
        r#"
package java.util.function;
public interface Predicate<T> {
    boolean test(T t);
}
"#,
    ),
    (
        "java/util/function/Function.java",
        r#"
package java.util.function;
public interface Function<T, R> {
    R apply(T t);
}
"#,
    ),
    (
        "java/util/stream/Stream.java",
        r#"
package java.util.stream;
public interface Stream<T> {
    Stream<T> filter(java.util.function.Predicate<? super T> predicate);
    <R> Stream<R> map(java.util.function.Function<? super T, ? extends R> mapper);
    <R, A> R collect(Collector<? super T, A, R> collector);
}
"#,
    ),
    (
        "java/util/stream/Collector.java",
        r#"
package java.util.stream;
public interface Collector<T, A, R> {
}
"#,
    ),
    (
        "java/util/stream/Collectors.java",
        r#"
package java.util.stream;
public final class Collectors {
    public static <T> Collector<T, ?, java.util.List<T>> toList() { return null; }
}
"#,
    ),
    (
        "java/util/Optional.java",
        r#"
package java.util;
public final class Optional<T> {
    public T get() { return null; }
    public boolean isPresent() { return false; }
}
"#,
    ),
    (
        "java/util/ArrayList.java",
        r#"
package java.util;
public class ArrayList<E> implements List<E> {
    public E get(int index) { return null; }
    public int size() { return 0; }
    public boolean isEmpty() { return true; }
    public boolean add(E e) { return true; }
}
"#,
    ),
];

pub struct SyntheticJdk {
    home: PathBuf,
    cache: PathBuf,
}

impl SyntheticJdk {
    /// `name` must be distinct per call site within a test binary: every test
    /// in the binary shares one process id, and that id is the only thing
    /// besides `name` that keeps two fixtures' temp directories apart.
    //
    // ponytail: two `build` calls with the same `name` in one process land on
    // the same temp directory, and `Drop` can then delete one fixture's
    // directory out from under the other's still-running test. The upgrade,
    // if that is ever actually hit, is an atomic counter or a uuid suffix
    // alongside `name` rather than relying on every call site remembering to
    // pick a distinct string.
    pub fn build(name: &str) -> Self {
        Self::build_padded(name, 0)
    }

    /// `build` plus `filler` trivial extra types, for the one check that has to
    /// observe an answer arriving *before* the read finishes.
    ///
    /// The padding is per-fixture rather than in `SOURCES` because eight of the
    /// nine fixture-owning checks want the read to be over in milliseconds.
    pub fn build_padded(name: &str, filler: usize) -> Self {
        let root = std::env::temp_dir().join(format!("javelin-jdk-{name}-{}", std::process::id()));
        let home = root.join("home");
        let cache = root.join("cache");
        std::fs::create_dir_all(home.join("lib")).expect("create the JDK home");
        std::fs::create_dir_all(&cache).expect("create the cache directory");
        std::fs::write(home.join("release"), "JAVA_VERSION=\"21.0.0\"\n").expect("write release");

        let file = std::fs::File::create(home.join("lib/src.zip")).expect("create src.zip");
        let mut archive = zip::ZipWriter::new(file);
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (entry, source) in SOURCES {
            archive.start_file(*entry, options).expect("start entry");
            archive.write_all(source.as_bytes()).expect("write entry");
        }
        for n in 0..filler {
            let entry = format!("filler/Filler{n}.java");
            archive.start_file(&entry, options).expect("start filler");
            let source = format!("package filler;\npublic class Filler{n} {{ int f; }}\n");
            archive
                .write_all(source.as_bytes())
                .expect("write filler entry");
        }
        archive.finish().expect("finish src.zip");

        Self { home, cache }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache
    }

    /// The handshake options naming this JDK and this cache.
    /// No build tool is run — the default, and said out loud because the demo
    /// project has a `pom.xml` and a check that ran Maven against it would
    /// depend on the network and the machine.
    pub fn options(&self) -> serde_json::Value {
        serde_json::json!({
            "jdk": { "home": self.home.to_string_lossy() },
            "index": { "cacheDir": self.cache.to_string_lossy() },
            "build": { "importOnStartup": false }
        })
    }

    /// [`options`](Self::options), with the build tool asked for the
    /// dependency classpath — for a project whose wrapper is a fake.
    pub fn options_with_dependencies(&self) -> serde_json::Value {
        let mut options = self.options();
        options["build"]["importOnStartup"] = serde_json::json!(true);
        options
    }
}

impl Drop for SyntheticJdk {
    fn drop(&mut self) {
        if let Some(root) = self.home.parent() {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_synthetic_jdk_looks_like_a_jdk_to_discovery() {
        let jdk = SyntheticJdk::build("looks-like-a-jdk");
        assert!(
            jdk.home().join("release").is_file(),
            "discovery needs a release file"
        );
        assert!(
            jdk.home().join("lib/src.zip").is_file(),
            "the reader needs sources"
        );
        let options = jdk.options();
        assert_eq!(
            options["jdk"]["home"],
            jdk.home().to_string_lossy().as_ref()
        );
        assert!(
            options["index"]["cacheDir"].is_string(),
            "a cache of its own, not the developer's"
        );
    }

    #[test]
    fn the_server_reads_the_synthetic_jdk_and_says_so() {
        use crate::harness::editor::Editor;
        use std::time::Duration;

        let jdk = SyntheticJdk::build("is-read");
        let mut editor = Editor::launch(&jdk.options());
        assert!(
            editor
                .logs()
                .wait_for("classpath ready", Duration::from_secs(30))
                .is_some(),
            "the classpath read never finished against the synthetic JDK"
        );
        let ready = editor
            .logs()
            .texts()
            .into_iter()
            .find(|line| line.contains("classpath ready"))
            .expect("just arrived")
            .to_string();
        assert!(
            ready.contains("source=\"src.zip\"") || ready.contains("source=src.zip"),
            "the first read must come from the archive, not a cache: {ready}"
        );
        // Exactly twenty, not merely non-zero: a count that only has to be
        // non-zero says nothing about several entries having failed to parse.
        // Twenty is the nineteen entries in `SOURCES` plus `Map.Entry`, which
        // the reader records as a fact of its own. The trailing space is what
        // keeps `facts=20` from matching `facts=200`. If you add an entry to `SOURCES`
        // — or a nested type to one — this number moves, and moving it is the
        // point.
        assert!(
            ready.contains("facts=20 "),
            "the archive should yield exactly twenty type facts — nineteen entries plus \
             Map.Entry; a different count means an entry failed to parse or SOURCES \
             changed without this number: {ready}"
        );
        // Establish the shutdown ordering rather than relying on it: without
        // this, `SyntheticJdk::drop` removes the JDK's directory — including
        // the open `src.zip` handle the server still holds — while the child
        // process is still running, which an open-file delete can fail on
        // silently rather than loudly.
        editor.shutdown();
    }
}
