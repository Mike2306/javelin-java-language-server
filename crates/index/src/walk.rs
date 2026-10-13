//! Finding the workspace's own Java sources.
//!
//! A recursive `read_dir` and a prune list, which is thirty lines and no new
//! dependency. What it deliberately does not know is where a build system
//! thinks the source roots are — that is Phase 4 — so "a source" here means
//! "a `.java` file under a directory nobody pruned".
//!
//! Nothing here parses. The walk answers *which* files and how old each one
//! is, so that [`crate::workspace`] can decide which of them are worth
//! opening at all.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use javelin_core::CancellationToken;

/// Directory names never descended into, on top of the rule that skips
/// anything beginning with `.`.
///
/// Every one of these is a build output directory, and the reason to skip
/// them is stronger than saving time: `target/classes` and
/// `target/generated-sources` hold copies of what `src/main/java` already
/// declares, so indexing them would make two declarations of one
/// fully-qualified name the normal case rather than the exceptional one.
///
/// ponytail: a hand-written list, so a Bazel `bazel-out` or an sbt `target`
/// nested somewhere unusual still gets walked. The upgrade is the build
/// system telling us its source roots, which is Phase 4 — not a longer list.
const PRUNED: [&str; 5] = ["target", "build", "out", "bin", "node_modules"];

/// One file the walk took, with what invalidation is decided on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    /// Nanoseconds since the Unix epoch, or `0` where the platform has no
    /// modification time to give. A clock that answers `0` for every file
    /// costs a reparse per session and no wrong answer.
    pub mtime_ns: i64,
    pub size: i64,
}

/// What a walk saw besides the files it took — the questions the
/// unresolved-type diagnostics ask before they trust the index to be whole.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Survey {
    /// A `.java` file was left out for its size, so a type it declares is
    /// missing from the index.
    pub skipped_large: bool,
    /// A Kotlin, Groovy or Scala source sits among the Java ones, and a Java
    /// file can name the types it declares — or an AspectJ one, which can
    /// declare types and add members to a Java type besides.
    pub foreign: bool,
    /// The simple names — file stems — of the `.java` files a build has
    /// generated under `target` or `build` that the walk did not index:
    /// every generator's but a processor's, and a processor's file too large
    /// to take.
    pub generated: std::collections::BTreeSet<String>,
    /// A generator plugin's output directory — not a processor's — was
    /// walked: the generator has run.
    pub generator_output: bool,
    /// A directory the walk prunes by its name holds a `.java` file of its
    /// own — a package segment named `build` or `out`, not build output — so
    /// the types in it are missing from the index.
    ///
    /// Below `src/` those names are packages and are walked; this is one
    /// outside it.
    ///
    /// ponytail: only a file directly inside the pruned directory is seen,
    /// not one in a subpackage below it. The upgrade is pruning build output
    /// by its position beside a build file rather than by its name.
    pub hidden: bool,
    /// Directories whose sources compile against a classpath the dependency
    /// reader does not ask for: one holding an Ant, Ivy, Bazel or sbt build,
    /// and a whole root with no Maven or Gradle build that holds a JAR, an
    /// Eclipse `.classpath`, an IntelliJ module or library, or a VS Code
    /// `referencedLibraries` setting.
    pub unasked: std::collections::BTreeSet<PathBuf>,
    /// Directories holding a `lombok.config` — under a root or above one —
    /// that sets a key changing what Lombok generates or what it calls it:
    /// `lombok.accessors.fluent`, `lombok.getter.noIsPrefix`, an `import` of
    /// another file. What Lombok generates below one is not believed.
    pub lombok_renamed: std::collections::BTreeSet<PathBuf>,
    /// Directories holding a `lombok.config` — under a root or above one —
    /// that names the field a log annotation writes, or stops the search
    /// for one there.
    pub lombok_log: std::collections::BTreeMap<PathBuf, LombokLog>,
}

/// What one `lombok.config` says about the field `@Slf4j` and its siblings
/// write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LombokLog {
    /// `lombok.log.fieldName`, or `log` where it is cleared.
    pub field_name: Option<String>,
    /// `config.stopBubbling = true`: no file above this one is read.
    pub stop_bubbling: bool,
}

impl LombokLog {
    /// The name of the field a log annotation writes in a file at `path`,
    /// by the configs in `configs` — the closest one naming it, short of one
    /// that stops the search (Lombok's own reading).
    pub fn field_name<'a>(
        configs: &'a std::collections::BTreeMap<PathBuf, LombokLog>,
        path: &Path,
    ) -> &'a str {
        for directory in path.ancestors().skip(1) {
            if let Some(config) = configs.get(directory) {
                if let Some(name) = &config.field_name {
                    return name;
                }
                if config.stop_bubbling {
                    break;
                }
            }
        }
        "log"
    }

    /// What the `lombok.config` at `path` says, where it says anything.
    fn read(path: &Path) -> Option<Self> {
        if !std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file()) {
            return None;
        }
        let text = std::fs::read_to_string(path).ok()?;
        let mut found = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if let Some(key) = line.strip_prefix("clear ") {
                if key.trim().eq_ignore_ascii_case("lombok.log.fieldName") {
                    found.field_name = Some("log".to_owned());
                }
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            if key.eq_ignore_ascii_case("lombok.log.fieldName") && !value.is_empty() {
                found.field_name = Some(value.to_owned());
            } else if key.eq_ignore_ascii_case("config.stopBubbling") {
                found.stop_bubbling = value.eq_ignore_ascii_case("true");
            }
        }
        (found != Self::default()).then_some(found)
    }
}

/// A build whose classpath nobody asks for, wherever it sits.
fn is_other_build(name: &str) -> bool {
    matches!(
        name,
        "build.xml"
            | "ivy.xml"
            | "BUILD"
            | "BUILD.bazel"
            | "WORKSPACE"
            | "WORKSPACE.bazel"
            | "MODULE.bazel"
            | "build.sbt"
    )
}

/// A classpath an IDE or a folder of JARs holds, which only matters where no
/// Maven or Gradle build answers for it.
fn is_loose_classpath(name: &str) -> bool {
    name == ".classpath"
        || name.ends_with(".iml")
        || (name.ends_with(".jar") && name != "gradle-wrapper.jar" && name != "maven-wrapper.jar")
}

fn is_maven_or_gradle(name: &str) -> bool {
    name == "pom.xml" || name.ends_with(".gradle") || name.ends_with(".gradle.kts")
}

/// An IDE's library settings at `root`, in the dot-directories the walk skips.
fn configures_libraries(root: &Path) -> bool {
    root.join(".idea/libraries").is_dir()
        || std::fs::read_to_string(root.join(".vscode/settings.json"))
            .is_ok_and(|settings| settings.contains("referencedLibraries"))
}

/// Where an annotation processor writes its output below a pruned build
/// directory — Gradle's, and Maven's for main and for test — which the walk
/// takes as a source root of its own.
///
/// Walked because a reader navigates into a MapStruct `*MapperImpl` as
/// often as into what it implements. What a build left there may be stale, so a type in it never makes a
/// closure whole — see `ClasspathExternals::source_supertypes` — and a
/// declaration the sources make wins over a generated copy of it.
///
/// After the processors' three come protobuf's, OpenAPI Generator's and
/// jOOQ's default output, under the same rules: what a processor writes and
/// what a generator plugin writes are equally a build's leftovers.
///
/// ponytail: by default directory, so a generator configured elsewhere — or
/// Swagger Codegen, JAXB, CXF, Avro, ANTLR — is a name the survey silences
/// at best rather than a place to jump; and a stale Maven copy left beside
/// Gradle's output is two copies of one type — definition and hover answer
/// nothing, while implementation, the type hierarchy and workspace symbol
/// list both. The upgrade is asking the build tool for its generated source
/// roots, which also says which of two build directories is live.
const PROCESSOR_OUTPUT: [(&str, &[&str]); 12] = [
    ("build", &["generated", "sources", "annotationProcessor"]),
    ("target", &["generated-sources", "annotations"]),
    ("target", &["generated-test-sources", "test-annotations"]),
    // protobuf's Gradle plugin, by its old and its new name, and its Maven
    // plugins.
    ("build", &["generated", "source", "proto"]),
    ("build", &["generated", "sources", "proto"]),
    ("target", &["generated-sources", "protobuf"]),
    ("target", &["generated-test-sources", "protobuf"]),
    // OpenAPI Generator's, which writes a whole project there.
    ("build", &["generate-resources", "main"]),
    ("target", &["generated-sources", "openapi"]),
    // jOOQ: nu.studer's Gradle plugin, jOOQ's own; its Maven plugin.
    ("build", &["generated-src", "jooq"]),
    ("build", &["generated-sources", "jooq"]),
    ("target", &["generated-sources", "jooq"]),
];

/// What the first directory below the build one starts with in every
/// [`PROCESSOR_OUTPUT`] entry.
const GENERATE: &[u8] = b"generate";

/// How many of [`PROCESSOR_OUTPUT`]'s entries, from the first, are an
/// annotation processor's.
const PROCESSORS: usize = 3;

/// Directories under a pruned build directory whose sources a build
/// generated. The walk indexes [`PROCESSOR_OUTPUT`] below them; the stem of
/// every other `.java` file below them is reported in [`Survey::generated`],
/// a name the index never read.
const GENERATED: [&str; 5] = [
    "generated-sources",
    "generated-test-sources",
    "generated",
    "generated-src",
    "generate-resources",
];

/// [`java_sources`], and what the walk saw besides.
pub fn java_sources_surveyed(
    roots: &[PathBuf],
    max_file_size_bytes: u64,
    cancel: &CancellationToken,
) -> (Vec<Found>, Survey) {
    let mut survey = Survey::default();
    let found = walk(roots, max_file_size_bytes, cancel, Some(&mut survey));
    (found, survey)
}

/// Every `.java` file under `roots`, pruned and size-limited.
///
/// Symlinks — to files and to directories alike — are not followed. A visited
/// set guarding against a link cycle is code to make a case work that Javelin
/// has no reason to support, and following one would let a file be indexed
/// twice under two paths.
///
/// A directory that cannot be read is logged and skipped: an unreadable
/// `node_modules` nobody owns must not cost the whole index.
pub fn java_sources(
    roots: &[PathBuf],
    max_file_size_bytes: u64,
    cancel: &CancellationToken,
) -> Vec<Found> {
    walk(roots, max_file_size_bytes, cancel, None)
}

fn walk(
    roots: &[PathBuf],
    max_file_size_bytes: u64,
    cancel: &CancellationToken,
    mut survey: Option<&mut Survey>,
) -> Vec<Found> {
    let mut found = Vec::new();
    // Per root: whether a Maven or Gradle build sits in it, and whether a
    // loose classpath does.
    let mut builds = vec![(false, false); roots.len()];
    // Per directory: its root, whether it is below a package root, whether
    // it is in a processor's output, and whether that is a generator's.
    let mut stack: Vec<(PathBuf, usize, bool, bool, bool)> = roots
        .iter()
        .enumerate()
        .map(|(index, root)| (root.clone(), index, false, false, false))
        .collect();
    while let Some((dir, root, below_src, output, generator)) = stack.pop() {
        if cancel.is_cancelled() {
            return found;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) => {
                tracing::debug!(dir = %dir.display(), %err, "workspace: directory skipped");
                continue;
            }
        };
        for entry in entries.flatten() {
            // `file_type` is the one that does not follow the link, which is
            // what makes this the symlink check as well as the kind check.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_str().unwrap_or_default();
            if kind.is_dir() {
                if !is_pruned(&path, below_src) {
                    stack.push((path, root, below_src || name == "src", output, generator));
                    continue;
                }
                let mut outputs = Vec::new();
                for (at, (build, below)) in PROCESSOR_OUTPUT.iter().enumerate() {
                    if *build == name
                        && let Some(output) = real_directory(&path, below)
                    {
                        outputs.push((output, at >= PROCESSORS));
                    }
                }
                // A package root: below it `build` and `out` are package
                // names, exactly as below `src/`.
                stack.extend(
                    outputs
                        .iter()
                        .map(|(output, generator)| (output.clone(), root, true, true, *generator)),
                );
                if let Some(survey) = survey.as_deref_mut() {
                    let walked: Vec<PathBuf> =
                        outputs.iter().map(|(output, _)| output.clone()).collect();
                    if matches!(name, "target" | "build") {
                        for generated in GENERATED {
                            generated_names(&path.join(generated), &walked, &mut survey.generated);
                        }
                    }
                    if matches!(name, "target" | "build" | "out" | "bin")
                        && holds_java_source(&path)
                    {
                        survey.hidden = true;
                    }
                }
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            if is_java_source(&path) {
                // A generator ran: its output is on disk, not only the
                // directory Gradle makes before the task.
                if generator && let Some(survey) = survey.as_deref_mut() {
                    survey.generator_output = true;
                }
                let stem = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_owned);
                match measured(path, max_file_size_bytes) {
                    Some(file) => found.push(file),
                    None => {
                        if let Some(survey) = survey.as_deref_mut() {
                            // A processor's output the walk cannot take is a
                            // name it never read, like any generator's — not
                            // a hole in the hand-written sources.
                            match stem.filter(|_| output) {
                                Some(stem) => {
                                    survey.generated.insert(stem);
                                }
                                None => survey.skipped_large = true,
                            }
                        }
                    }
                }
                continue;
            }
            // What else a build left beside its output says nothing about
            // the project.
            if output {
                continue;
            }
            let Some(survey) = survey.as_deref_mut() else {
                continue;
            };
            if path.extension().is_some_and(|extension| {
                matches!(extension.to_str(), Some("kt" | "groovy" | "scala" | "aj"))
            }) {
                survey.foreign = true;
            } else if name == "lombok.config" {
                if lombok_config_renames(&path) {
                    survey.lombok_renamed.insert(dir.clone());
                }
                if let Some(log) = LombokLog::read(&path) {
                    survey.lombok_log.insert(dir.clone(), log);
                }
            } else if is_other_build(name) {
                survey.unasked.insert(dir.clone());
            } else if is_maven_or_gradle(name) {
                builds[root].0 = true;
            } else if is_loose_classpath(name) {
                builds[root].1 = true;
            }
        }
    }
    if let Some(survey) = survey {
        // Lombok reads every `lombok.config` from a file's directory up to
        // the filesystem's root.
        // ponytail: one above the roots is read here only, since the watcher's
        // `**/lombok.config` is under them — a mid-session change to it is
        // seen at the next re-walk, and until then a renamed accessor can be
        // flagged as missing. Upgrade: register each ancestor's path too.
        for root in roots {
            for above in root.ancestors().skip(1) {
                if lombok_config_renames(&above.join("lombok.config")) {
                    survey.lombok_renamed.insert(above.to_path_buf());
                }
                if let Some(log) = LombokLog::read(&above.join("lombok.config")) {
                    survey.lombok_log.insert(above.to_path_buf(), log);
                }
            }
        }
        for (root, (maven_or_gradle, loose)) in roots.iter().zip(builds) {
            if !maven_or_gradle && (loose || configures_libraries(root)) {
                survey.unasked.insert(root.clone());
            }
        }
    }
    found
}

/// Whether a path is one the walk would have taken, judged by name alone: a
/// `.java` source under one of `roots` with no pruned directory between.
///
/// For an event about one file — a save, a watcher's report, a buffer — which
/// must not index what a full walk would have skipped, and must not stat a
/// file that may already be gone.
pub fn admits(roots: &[PathBuf], path: &Path) -> bool {
    if !is_java_source(path) {
        return false;
    }
    roots.iter().any(|root| {
        // `strip_prefix` compares components, so `/w/project-two` is not
        // under `/w/project`.
        path.strip_prefix(root).is_ok_and(|relative| {
            let directories: Vec<&str> = relative
                .parent()
                .into_iter()
                .flat_map(Path::components)
                .map(component_name)
                .collect();
            !passes_a_pruned(&directories, false)
                || processor_output_split(&directories).is_some_and(|(before, after)| {
                    !passes_a_pruned(before, false) && !passes_a_pruned(after, true)
                })
        })
    })
}

/// Whether `path` — a classpath entry — is something a Maven or Gradle build
/// under one of `roots` wrote: it lies below a `target` or a `build` directory
/// the walk prunes. By name, as the walk prunes; `out` and `bin` are left out
/// because a checked-in JAR lives there as often as a build's.
pub fn is_build_output(roots: &[PathBuf], path: &Path) -> bool {
    roots.iter().any(|root| {
        path.strip_prefix(root).is_ok_and(|relative| {
            let mut below_src = false;
            relative
                .parent()
                .into_iter()
                .flat_map(Path::components)
                .map(component_name)
                // A dot-directory — a repository local to the project, say —
                // is never output, whatever lies below it.
                .take_while(|name| !name.starts_with('.'))
                .any(|name| {
                    let output = !below_src && matches!(name, "target" | "build");
                    below_src |= name == "src";
                    output
                })
        })
    })
}

/// Whether `path` lies in an annotation processor's output directory — one of
/// `PROCESSOR_OUTPUT` — wherever it is. By name alone, for a reader that
/// holds an indexed file and no roots: the walk took it, so the roots
/// already agreed.
pub fn is_processor_output(path: &Path) -> bool {
    // Every entry's directories below the build one start with `generate`,
    // and the member-reference pass asks once per frame per lookup: most
    // paths are answered here without being split into components.
    if !path
        .as_os_str()
        .as_encoded_bytes()
        .windows(GENERATE.len())
        .any(|window| window == GENERATE)
    {
        return false;
    }
    let directories: Vec<&str> = path
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    (0..directories.len()).any(|start| processor_output_split(&directories[start..]).is_some())
}

/// Whether creating or deleting `path` under one of `roots` can add or remove
/// a processor's output: it is a build directory the walk prunes on the way
/// into one of `PROCESSOR_OUTPUT`, or lies inside one. A `clean` deletes
/// the build directory and a client may report that alone.
pub fn reaches_processor_output(roots: &[PathBuf], path: &Path) -> bool {
    roots.iter().any(|root| {
        path.strip_prefix(root).is_ok_and(|relative| {
            let names: Vec<&str> = relative.components().map(component_name).collect();
            let mut below_src = false;
            for (at, name) in names.iter().enumerate() {
                if is_pruned(Path::new(name), below_src) {
                    let rest = &names[at + 1..];
                    return PROCESSOR_OUTPUT.iter().any(|(build, below)| {
                        build == name && (below.starts_with(rest) || rest.starts_with(below))
                    });
                }
                below_src |= *name == "src";
            }
            false
        })
    })
}

/// The directories before the first [`PROCESSOR_OUTPUT`] in `directories`,
/// when it begins one the walk would reach — at the first directory the walk
/// prunes — and the directories after it.
fn processor_output_split<'a>(
    directories: &'a [&'a str],
) -> Option<(&'a [&'a str], &'a [&'a str])> {
    let mut below_src = false;
    for (at, name) in directories.iter().enumerate() {
        if is_pruned(Path::new(name), below_src) {
            return PROCESSOR_OUTPUT.iter().find_map(|(build, below)| {
                let rest = &directories[at + 1..];
                (build == name && rest.starts_with(below))
                    .then(|| (&directories[..at], &rest[below.len()..]))
            });
        }
        below_src |= *name == "src";
    }
    None
}

/// `base` joined with `below`, when every step is a directory and none is a
/// symlink — the walk follows none.
fn real_directory(base: &Path, below: &[&str]) -> Option<PathBuf> {
    let mut path = base.to_path_buf();
    for name in below {
        path.push(name);
        if !std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
            return None;
        }
    }
    Some(path)
}

/// A file's size and time, if it is a regular file — not a symlink, which the
/// walk never follows — and within the limit: what the walk records of every
/// file it takes.
pub fn measure(path: &Path, max_file_size_bytes: u64) -> Option<Found> {
    let kind = std::fs::symlink_metadata(path).ok()?.file_type();
    if !kind.is_file() {
        return None;
    }
    measured(path.to_path_buf(), max_file_size_bytes)
}

/// Whether a path — a file or a directory, there or already gone — lies under
/// one of `roots` with no pruned directory on the way, itself included: where
/// a change can have moved what the walk would find.
pub fn within(roots: &[PathBuf], path: &Path) -> bool {
    roots.iter().any(|root| {
        path.strip_prefix(root).is_ok_and(|relative| {
            let names: Vec<&str> = relative.components().map(component_name).collect();
            !passes_a_pruned(&names, false)
        })
    })
}

/// Whether `path` is a `.java` file a build generated under one of `roots`:
/// below a `target` or `build` directory, in one of its generated-source
/// directories. The walk does not take it, and records its name.
pub fn is_generated_source(roots: &[PathBuf], path: &Path) -> bool {
    if !is_java_source(path) {
        return false;
    }
    roots.iter().any(|root| {
        path.strip_prefix(root).is_ok_and(|relative| {
            let names: Vec<&str> = relative
                .components()
                .filter_map(|component| component.as_os_str().to_str())
                .collect();
            names
                .windows(2)
                .any(|pair| matches!(pair[0], "target" | "build") && GENERATED.contains(&pair[1]))
        })
    })
}

/// Whether the `lombok.config` at `path` sets a key that changes what Lombok
/// generates or what it names it — every key of the accessors, the getter,
/// setter, builder, `@With`, `@Data`, `@Value`, `@Singular` and
/// `@FieldDefaults` families except a `flagUsage`, which only warns — or
/// imports another file, which could. `false` where there is no file.
fn lombok_config_renames(path: &Path) -> bool {
    const FAMILIES: [&str; 9] = [
        "lombok.accessors.",
        "lombok.getter.",
        "lombok.setter.",
        "lombok.builder.",
        "lombok.with.",
        "lombok.data.",
        "lombok.value.",
        "lombok.singular.",
        "lombok.fielddefaults.",
    ];
    if !std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }
    // Unreadable is a file whose keys nobody knows.
    let Ok(text) = std::fs::read_to_string(path) else {
        return true;
    };
    text.lines().any(|line| {
        let line = line.trim();
        let mut words = line.split_whitespace();
        let key = match words.next() {
            None => return false,
            Some(comment) if comment.starts_with('#') => return false,
            Some("import") => return true,
            Some("clear") => words.next().unwrap_or_default(),
            Some(_) => line
                .split(|c: char| c == '=' || c == '+' || c == '-' || c.is_whitespace())
                .next()
                .unwrap_or_default(),
        }
        .to_ascii_lowercase();
        FAMILIES.iter().any(|family| key.starts_with(family)) && !key.ends_with(".flagusage")
    })
}

/// Whether `dir` holds a `.java` file directly — a package, not build output.
fn holds_java_source(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_file()) && is_java_source(&entry.path())
        })
    })
}

/// The stems of every `.java` file under `dir`, no symlink followed, outside
/// the `walked` processor output.
fn generated_names(dir: &Path, walked: &[PathBuf], names: &mut std::collections::BTreeSet<String>) {
    if walked.iter().any(|output| output == dir) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            generated_names(&path, walked, names);
        } else if kind.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "java")
            && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
        {
            names.insert(stem.to_owned());
        }
    }
}

/// A path component's name, with one that is not UTF-8 read as `.` — a name
/// the walk prunes, as it prunes every directory whose name it cannot read.
fn component_name(component: std::path::Component<'_>) -> &str {
    component.as_os_str().to_str().unwrap_or(".")
}

/// Whether a walk down `names` meets a directory it prunes, starting below a
/// package root or not.
fn passes_a_pruned(names: &[&str], mut below_src: bool) -> bool {
    for name in names {
        if is_pruned(Path::new(name), below_src) {
            return true;
        }
        below_src |= *name == "src";
    }
    false
}

/// Below a `src/` directory, `target`, `build`, `out` and `bin` are package
/// names — `com.example.build` — since no build tool writes its output there.
fn is_pruned(dir: &Path, below_src: bool) -> bool {
    let Some(name) = dir.file_name().and_then(|name| name.to_str()) else {
        return true;
    };
    name.starts_with('.') || (PRUNED.contains(&name) && !(below_src && name != "node_modules"))
}

/// `module-info.java` and `package-info.java` declare no type and are skipped
/// for the same reason `javelin-classpath` skips them in `src.zip`.
fn is_java_source(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.ends_with(".java") && name != "module-info.java" && name != "package-info.java"
}

fn measured(path: PathBuf, max_file_size_bytes: u64) -> Option<Found> {
    let metadata = std::fs::metadata(&path)
        .inspect_err(|err| tracing::debug!(path = %path.display(), %err, "workspace: no metadata"))
        .ok()?;
    if metadata.len() > max_file_size_bytes {
        tracing::debug!(path = %path.display(), size = metadata.len(), "workspace: file too large");
        return None;
    }
    let mtime_ns = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|since| i64::try_from(since.as_nanos()).ok())
        .unwrap_or(0);
    Some(Found {
        path,
        mtime_ns,
        size: i64::try_from(metadata.len()).unwrap_or(i64::MAX),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree(PathBuf);

    #[test]
    fn every_processor_output_directory_passes_the_fast_reject() {
        for (build, below) in PROCESSOR_OUTPUT {
            assert!(
                below[0].as_bytes().starts_with(GENERATE),
                "{build}/{below:?}"
            );
            let path: PathBuf = ["/w", build]
                .iter()
                .chain(below)
                .chain(&["A.java"])
                .collect();
            assert!(is_processor_output(&path), "{}", path.display());
        }
        assert!(!is_processor_output(Path::new("/w/build/classes/A.java")));
    }

    #[test]
    fn a_jar_below_a_modules_build_directory_is_the_workspaces_own_output() {
        let roots = [PathBuf::from("/w")];
        assert!(is_build_output(
            &roots,
            Path::new("/w/core/build/libs/core.jar")
        ));
        assert!(is_build_output(
            &roots,
            Path::new("/w/core/target/core-1.jar")
        ));
        assert!(!is_build_output(&roots, Path::new("/w/libs/acme.jar")));
        assert!(!is_build_output(&roots, Path::new("/w/bin/acme.jar")));
        assert!(!is_build_output(&roots, Path::new("/m2/x/build/x.jar")));
        assert!(!is_build_output(
            &roots,
            Path::new("/w/src/main/java/com/build/x.jar")
        ));
        assert!(!is_build_output(&roots, Path::new("/w/build.jar")));
        assert!(!is_build_output(
            &roots,
            Path::new("/w/.m2/repository/com/x/build/1/build-1.jar")
        ));
    }

    impl Tree {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "javelin-walk-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("temp tree");
            Self(path)
        }

        fn file(&self, relative: &str, text: &str) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("dirs");
            std::fs::write(&path, text).expect("write");
            path
        }

        fn roots(&self) -> Vec<PathBuf> {
            vec![self.0.clone()]
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn names(found: &[Found]) -> Vec<String> {
        let mut names: Vec<String> = found
            .iter()
            .map(|file| {
                file.path
                    .file_name()
                    .expect("a name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_java_file_under_a_root_is_found() {
        let tree = Tree::new("found");
        tree.file("src/main/java/com/example/A.java", "class A {}");
        let found = java_sources(&tree.roots(), 1 << 20, &CancellationToken::new());
        assert_eq!(names(&found), vec!["A.java"]);
    }

    #[test]
    fn build_output_and_dot_directories_are_not_descended_into() {
        let tree = Tree::new("pruned");
        tree.file("src/A.java", "class A {}");
        tree.file("target/classes/A.java", "class A {}");
        tree.file("build/B.java", "class B {}");
        tree.file("node_modules/c/C.java", "class C {}");
        tree.file(".git/D.java", "class D {}");
        let found = java_sources(&tree.roots(), 1 << 20, &CancellationToken::new());
        assert_eq!(names(&found), vec!["A.java"]);
    }

    /// A processor's output is a source root of its own; the rest of build
    /// output — a generator's, a copy in `classes` — stays pruned.
    #[test]
    fn an_annotation_processors_output_is_walked_and_other_build_output_is_not() {
        let tree = Tree::new("processor-output");
        let walked = [
            tree.file("src/A.java", "class A {}"),
            tree.file(
                "m/build/generated/sources/annotationProcessor/java/main/p/AMapperImpl.java",
                "class AMapperImpl {}",
            ),
            tree.file(
                "target/generated-sources/annotations/p/BImpl.java",
                "class BImpl {}",
            ),
            tree.file(
                "target/generated-test-sources/test-annotations/p/CImpl.java",
                "class CImpl {}",
            ),
            // A package named like build output, below a processor's root.
            tree.file(
                "target/generated-sources/annotations/p/build/E.java",
                "class E {}",
            ),
        ];
        let pruned = [
            tree.file(
                "build/generated/sources/headers/main/p/D.java",
                "class D {}",
            ),
            tree.file("target/classes/p/F.java", "class F {}"),
            tree.file(
                "target/generated-sources/annotations/.cache/G.java",
                "class G {}",
            ),
        ];
        let (found, survey) =
            java_sources_surveyed(&tree.roots(), 1 << 20, &CancellationToken::new());
        assert_eq!(
            names(&found),
            vec![
                "A.java",
                "AMapperImpl.java",
                "BImpl.java",
                "CImpl.java",
                "E.java"
            ]
        );
        for path in &walked {
            assert!(admits(&tree.roots(), path), "{}", path.display());
        }
        for path in &pruned {
            assert!(!admits(&tree.roots(), path), "{}", path.display());
        }
        assert!(is_processor_output(&walked[1]) && is_processor_output(&walked[4]));
        assert!(!is_processor_output(&walked[0]) && !is_processor_output(&pruned[0]));
        // The names a build generated and the walk did not index are
        // reported; the ones it indexed resolve.
        assert!(survey.generated.contains("D") && !survey.generated.contains("AMapperImpl"));
    }

    /// protobuf's, OpenAPI Generator's and jOOQ's default output is walked as
    /// a processor's is; another generator's below the same build
    /// directories is a name the index never read.
    #[test]
    fn a_generators_default_output_is_walked_as_a_processors_is() {
        let tree = Tree::new("generator-output");
        let walked = [
            "build/generated/source/proto/main/java/p/A.java",
            "build/generated/sources/proto/main/grpc/p/B.java",
            "target/generated-sources/protobuf/java/p/C.java",
            "target/generated-test-sources/protobuf/java/p/D.java",
            "m/build/generate-resources/main/src/main/java/p/E.java",
            "target/generated-sources/openapi/src/main/java/p/F.java",
            "build/generated-src/jooq/main/p/G.java",
            "build/generated-sources/jooq/p/H.java",
            "target/generated-sources/jooq/p/I.java",
        ]
        .map(|path| tree.file(path, "class X {}"));
        let silenced = [
            "build/generated-src/antlr/main/p/J.java",
            "build/generate-resources/other/p/K.java",
            "target/generated-sources/xjc/p/L.java",
        ]
        .map(|path| tree.file(path, "class X {}"));
        let (found, survey) =
            java_sources_surveyed(&tree.roots(), 1 << 20, &CancellationToken::new());
        assert_eq!(
            names(&found),
            vec![
                "A.java", "B.java", "C.java", "D.java", "E.java", "F.java", "G.java", "H.java",
                "I.java"
            ]
        );
        for path in &walked {
            assert!(admits(&tree.roots(), path), "{}", path.display());
            assert!(is_processor_output(path), "{}", path.display());
            assert!(
                reaches_processor_output(&tree.roots(), path),
                "{}",
                path.display()
            );
        }
        for path in &silenced {
            assert!(!admits(&tree.roots(), path), "{}", path.display());
            assert!(
                is_generated_source(&tree.roots(), path),
                "{}",
                path.display()
            );
        }
        let mut generated: Vec<&str> = survey.generated.iter().map(String::as_str).collect();
        generated.sort_unstable();
        assert_eq!(generated, vec!["J", "K", "L"]);
        assert!(survey.generator_output);

        // A generator's directory with nothing in it is a task that has not
        // run, which Gradle creates before running it.
        let tree = Tree::new("generator-output-empty");
        std::fs::create_dir_all(tree.0.join("build/generated-src/jooq/main/p")).unwrap();
        let (_, survey) = java_sources_surveyed(&tree.roots(), 1 << 20, &CancellationToken::new());
        assert!(!survey.generator_output);

        // A processor's output is not a generator's.
        let tree = Tree::new("processor-output-only");
        tree.file(
            "build/generated/sources/annotationProcessor/java/main/p/M.java",
            "class M {}",
        );
        let (_, survey) = java_sources_surveyed(&tree.roots(), 1 << 20, &CancellationToken::new());
        assert!(!survey.generator_output);
    }

    /// One large generated file — a Dagger component — is a name the index
    /// never read, not a hole in the sources that would refuse the world;
    /// and what else a build left in its output says nothing about the
    /// project.
    #[test]
    fn a_processors_output_never_marks_the_survey() {
        let tree = Tree::new("processor-output-survey");
        tree.file("src/A.java", "class A {}");
        let output = "build/generated/sources/annotationProcessor/java/main/p/";
        tree.file(
            &format!("{output}DaggerBig.java"),
            &format!("class DaggerBig {{ {} }}", " ".repeat(64)),
        );
        tree.file(&format!("{output}Small.java"), "class Small {}");
        tree.file(&format!("{output}Stray.kt"), "class Stray");
        let (found, survey) = java_sources_surveyed(&tree.roots(), 32, &CancellationToken::new());
        assert_eq!(names(&found), vec!["A.java", "Small.java"]);
        assert!(!survey.skipped_large && !survey.foreign);
        assert!(survey.generated.contains("DaggerBig") && !survey.generated.contains("Small"));
    }

    #[test]
    fn a_file_that_is_not_java_is_not_found() {
        let tree = Tree::new("not-java");
        tree.file("src/A.java", "class A {}");
        tree.file("src/pom.xml", "<project/>");
        tree.file("src/A.class", "");
        tree.file("src/package-info.java", "package com.example;");
        let found = java_sources(&tree.roots(), 1 << 20, &CancellationToken::new());
        assert_eq!(names(&found), vec!["A.java"]);
    }

    #[test]
    fn a_file_over_the_size_limit_is_not_found() {
        let tree = Tree::new("too-big");
        tree.file("src/Small.java", "class A {}");
        tree.file("src/Big.java", &"// padding\n".repeat(200));
        let found = java_sources(&tree.roots(), 64, &CancellationToken::new());
        assert_eq!(names(&found), vec!["Small.java"]);
    }

    #[test]
    fn a_files_size_and_modification_time_come_back_with_it() {
        let tree = Tree::new("stat");
        tree.file("src/A.java", "class A {}");
        let found = java_sources(&tree.roots(), 1 << 20, &CancellationToken::new());
        assert_eq!(found[0].size, 10);
        assert!(found[0].mtime_ns > 0, "a real file has a modification time");
    }

    #[test]
    fn admits_takes_a_source_under_a_root_and_refuses_pruned_and_outside_paths() {
        let roots = vec![PathBuf::from("/w/project")];
        assert!(admits(&roots, Path::new("/w/project/src/com/A.java")));
        assert!(!admits(
            &roots,
            Path::new("/w/project/target/classes/A.java")
        ));
        assert!(!admits(&roots, Path::new("/w/project/.git/A.java")));
        assert!(!admits(
            &roots,
            Path::new("/w/project/src/package-info.java")
        ));
        assert!(!admits(&roots, Path::new("/w/project/src/pom.xml")));
        assert!(!admits(&roots, Path::new("/w/other/A.java")));
        assert!(
            !admits(&roots, Path::new("/w/project-two/A.java")),
            "a sibling sharing a prefix"
        );
    }

    #[test]
    fn admits_agrees_with_the_walk_on_a_real_tree() {
        let tree = Tree::new("admits");
        let taken = tree.file("src/A.java", "class A {}");
        let pruned = tree.file("build/B.java", "class B {}");
        let found = java_sources(&tree.roots(), 1 << 20, &CancellationToken::new());
        assert_eq!(found.len(), 1);
        assert!(admits(&tree.roots(), &taken));
        assert!(!admits(&tree.roots(), &pruned));
    }

    #[test]
    fn a_cancelled_walk_stops_rather_than_finishing() {
        let tree = Tree::new("cancelled");
        tree.file("src/A.java", "class A {}");
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(java_sources(&tree.roots(), 1 << 20, &cancel).is_empty());
    }

    #[test]
    fn a_root_that_does_not_exist_is_empty_rather_than_an_error() {
        let missing = std::env::temp_dir().join("javelin-walk-nothing-here-at-all");
        let found = java_sources(&[missing], 1 << 20, &CancellationToken::new());
        assert!(found.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_is_not_followed() {
        let tree = Tree::new("symlink");
        tree.file("real/A.java", "class A {}");
        std::os::unix::fs::symlink(tree.0.join("real"), tree.0.join("link")).expect("symlink");
        let found = java_sources(&tree.roots(), 1 << 20, &CancellationToken::new());
        assert_eq!(
            names(&found),
            vec!["A.java"],
            "the link's copy is not taken"
        );
    }

    /// What diagnostics need to know about a walk besides what it found:
    /// whether it left a Java file out, whether a Java file could name a
    /// type in a language it does not read, and which types a build has
    /// generated where the walk does not look.
    /// An AspectJ aspect declares types, and can add members to a Java one.
    #[test]
    fn an_aspect_is_a_foreign_source() {
        let tree = Tree::new("survey-aspect");
        tree.file("src/A.java", "class A {}");
        tree.file("src/main/aspect/Tracing.aj", "aspect Tracing {}");
        let (_, survey) = java_sources_surveyed(&tree.roots(), 1 << 20, &CancellationToken::new());
        assert!(survey.foreign);
    }

    #[test]
    fn a_survey_reports_skips_foreign_sources_and_generated_names() {
        let tree = Tree::new("survey");
        tree.file("src/A.java", "class A {}");
        let (found, survey) =
            java_sources_surveyed(&tree.roots(), 1 << 20, &CancellationToken::new());
        assert_eq!(found.len(), 1);
        assert_eq!(survey, Survey::default());

        tree.file("src/Big.java", &"x".repeat(64));
        tree.file("src/main/kotlin/K.kt", "class K");
        tree.file("build.gradle.kts", "plugins {}");
        tree.file(
            "target/generated-sources/annotations/p/ImmutableA.java",
            "class ImmutableA {}",
        );
        tree.file("build/generated/sources/p/A_.java", "class A_ {}");
        tree.file("target/classes/p/Copy.java", "class Copy {}");
        let (found, survey) = java_sources_surveyed(&tree.roots(), 32, &CancellationToken::new());
        assert_eq!(
            found.len(),
            2,
            "the large file is not taken, and a processor's output is"
        );
        assert!(survey.skipped_large);
        assert!(
            survey.foreign,
            "a Kotlin source; a Gradle script is not one"
        );
        let mut generated: Vec<&str> = survey.generated.iter().map(String::as_str).collect();
        generated.sort_unstable();
        assert_eq!(generated, vec!["A_"], "`ImmutableA` was indexed");
        assert!(!survey.hidden, "build output holds no source of its own");
        assert!(survey.unasked.is_empty(), "a Gradle build is asked");
    }

    /// Below `src/`, `build` is a package and is walked; outside it, a
    /// pruned directory holding sources is reported, since its types are
    /// missing.
    #[test]
    fn a_package_named_like_build_output_is_walked_below_src_and_reported_elsewhere() {
        let tree = Tree::new("hidden");
        tree.file(
            "src/main/java/p/build/Info.java",
            "package p.build; class Info {}",
        );
        tree.file("src/main/java/p/out/impl/Printer.java", "class Printer {}");
        tree.file("src/main/webapp/node_modules/x/X.java", "class X {}");
        let (found, survey) =
            java_sources_surveyed(&tree.roots(), 1 << 20, &CancellationToken::new());
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(!survey.hidden);
        let root = &tree.roots()[0];
        assert!(admits(
            &tree.roots(),
            &root.join("src/main/java/p/build/Info.java")
        ));
        assert!(!admits(&tree.roots(), &root.join("target/p/Info.java")));

        tree.file("lib/build/Info.java", "class Info {}");
        let (_, survey) = java_sources_surveyed(&tree.roots(), 1 << 20, &CancellationToken::new());
        assert!(survey.hidden);
    }

    /// An Ant, Bazel or sbt build is a classpath nobody asks for wherever it
    /// sits; a JAR or an IDE's project file is one only where no Maven or
    /// Gradle build answers for the root.
    #[test]
    fn a_build_or_a_jar_nothing_asks_is_reported_where_it_applies() {
        for file in ["lib/commons.jar", "app.iml", ".classpath"] {
            let tree = Tree::new(&format!("unasked-{}", file.replace('/', "-")));
            tree.file("src/A.java", "class A {}");
            tree.file(file, "");
            let (_, survey) =
                java_sources_surveyed(&tree.roots(), 1 << 20, &CancellationToken::new());
            assert_eq!(survey.unasked, [tree.roots()[0].clone()].into(), "{file}");
            tree.file("pom.xml", "<project/>");
            let (_, survey) =
                java_sources_surveyed(&tree.roots(), 1 << 20, &CancellationToken::new());
            assert!(survey.unasked.is_empty(), "{file} beside a Maven build");
        }
        let tree = Tree::new("unasked-ant");
        tree.file("pom.xml", "<project/>");
        tree.file("tools/legacy/build.xml", "<project/>");
        let (_, survey) = java_sources_surveyed(&tree.roots(), 1 << 20, &CancellationToken::new());
        assert_eq!(
            survey.unasked,
            [tree.roots()[0].join("tools/legacy")].into()
        );

        let tree = Tree::new("unasked-idea");
        tree.file(".idea/libraries/guava.xml", "");
        let (_, survey) = java_sources_surveyed(&tree.roots(), 1 << 20, &CancellationToken::new());
        assert_eq!(survey.unasked, [tree.roots()[0].clone()].into());

        let tree = Tree::new("unasked-wrapper");
        tree.file("gradle/wrapper/gradle-wrapper.jar", "");
        let (_, survey) = java_sources_surveyed(&tree.roots(), 1 << 20, &CancellationToken::new());
        assert!(
            survey.unasked.is_empty(),
            "a wrapper is the build tool's own"
        );
    }

    #[test]
    fn a_generated_source_is_told_apart_by_its_position() {
        let root = PathBuf::from("/w");
        let roots = [root.clone()];
        assert!(is_generated_source(
            &roots,
            &root.join("target/generated-sources/annotations/p/ImmutableA.java")
        ));
        assert!(is_generated_source(
            &roots,
            &root.join("m/build/generated/sources/p/A_.java")
        ));
        assert!(!is_generated_source(&roots, &root.join("src/p/A.java")));
        assert!(!is_generated_source(
            &roots,
            &root.join("target/classes/p/A.java")
        ));
    }
}
