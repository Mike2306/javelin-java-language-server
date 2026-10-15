//! Asking Maven or Gradle for the dependency classpath.
//!
//! The tool is invoked, never reimplemented: mediation, BOMs, profiles and
//! version catalogues are the tool's, and a classpath that is subtly wrong is
//! the hardest kind to debug. Its answer is cached keyed by every build file's
//! modification time and size, so a session whose build files have not moved
//! starts no process at all.
//!
//! Running the tool executes the project's build logic — a Gradle script is a
//! program, a Maven build runs plugins. ADR 0005 records that, and
//! `build.importOnStartup` is the switch.

use std::collections::HashSet;
use std::hash::{Hash as _, Hasher as _};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use javelin_core::{CancellationToken, Error, Result};

use crate::BuildSystemKind;

/// How long the tool may run before it is killed. A cold Maven resolution
/// downloads the world; ten minutes is generous and still bounded.
const TIMEOUT: Duration = Duration::from_secs(600);

/// How often the child is polled for exit and the session for cancellation.
const POLL: Duration = Duration::from_millis(50);

/// Directories a build file under which is not the project's own: the same
/// ones the source walk prunes, since build output and dependencies hold copies.
const PRUNED: &[&str] = &["target", "build", "out", "bin", "node_modules"];

/// Whether `path`, relative to a root, is one of the files a classpath's
/// answer is keyed by — the walk [`build_files_key`] makes, by name: a build
/// file, or any file under `.mvn/`, `buildSrc/` or `build-logic/`, and never
/// one below build output or a dot-directory, nor a JAR, which a wrapper
/// downloads.
pub fn is_build_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if name.ends_with(".jar") {
        return false;
    }
    let mut all = false;
    for component in path.parent().into_iter().flat_map(Path::components) {
        match component.as_os_str().to_str() {
            Some(".mvn" | "buildSrc" | "build-logic") => all = true,
            Some(dir) if dir.starts_with('.') || PRUNED.contains(&dir) => return false,
            Some(_) => {}
            None => return false,
        }
    }
    all || is_build_file(name)
}

/// A file that can declare or pin a dependency: a POM, any Gradle script —
/// `buildSrc` convention plugins and applied scripts included — any version
/// catalogue, Gradle's properties, and either wrapper's. Everything under
/// `.mvn/` counts as well, which [`collect_build_files`] walks for it.
///
/// ponytail: a parent POM outside the opened folder is not read, so a
/// submodule opened on its own misses a version bump in its parent. The
/// upgrade is following `<relativePath>` up the chain.
fn is_build_file(name: &str) -> bool {
    name == "pom.xml"
        || name.ends_with(".gradle")
        || name.ends_with(".gradle.kts")
        || name.ends_with(".versions.toml")
        || name == "gradle.properties"
        || name == "gradle-wrapper.properties"
        || name == "maven-wrapper.properties"
}

/// Prints every entry of every resolvable compile classpath of every project
/// — `compileClasspath`, `testCompileClasspath` and each other source set's,
/// `integrationTestCompileClasspath` and `testFixturesCompileClasspath`
/// among them — one per line, for [`parse_gradle_output`], and
/// [`GRADLE_INCOMPLETE`] for a configuration that could not be resolved whole.
/// And each project's Java source directories, which are what it compiles.
///
/// Every line carries its project's path: with `org.gradle.parallel` two
/// projects' tasks interleave their output, so a header followed by entries
/// would give one project's JARs to another.
///
/// `project` is read at execution time, which the configuration cache refuses,
/// so the tool is run with `--no-configuration-cache`.
const GRADLE_INIT: &str = r#"allprojects {
    tasks.register('javelinClasspath') {
        doLast {
            // Each source set by what javac is handed, which a build may
            // extend past any configuration — `compileClasspath +=
            // configurations.testRuntimeClasspath`. Strict: a set whose
            // classpath cannot be resolved whole says so and is not judged.
            def sourceSets = project.extensions.findByType(SourceSetContainer)
            if (sourceSets != null) {
                sourceSets.each { sourceSet ->
                    def set = project.path + '/' + sourceSet.name
                    sourceSet.java.srcDirs.each {
                        println 'JAVELIN_SRC=' + set + '\t' + it.absolutePath
                    }
                    try {
                        sourceSet.compileClasspath.files.each {
                            println 'JAVELIN_SET=' + set + '\t' + it.absolutePath
                        }
                    } catch (Exception ignored) {
                        println 'JAVELIN_SET_FAILED=' + set
                    }
                }
            }
            project.configurations.findAll { configuration ->
                configuration.canBeResolved &&
                    (configuration.name == 'compileClasspath' ||
                        configuration.name.endsWith('CompileClasspath'))
            }.each { configuration ->
                // Lenient: one unresolvable dependency costs that one, not
                // the whole classpath — and says so, since the answer is then
                // not all there is.
                def artifacts = configuration.incoming.artifactView { lenient = true }.artifacts
                artifacts.artifactFiles.each {
                    println 'JAVELIN_CP=' + project.path + '\t' + it.absolutePath
                }
                if (!artifacts.failures.isEmpty()) {
                    println 'JAVELIN_INCOMPLETE=' + project.path + ':' + configuration.name
                }
            }
        }
    }
}
"#;

/// Plain console: `org.gradle.console=rich` in a project's properties would
/// draw progress into the pipe, around and over the printed lines.
pub(crate) const GRADLE_ARGS: [&str; 4] = [
    "-q",
    "--console=plain",
    "--continue",
    "--no-configuration-cache",
];

/// The line the init script prints for a configuration it resolved only in
/// part: a dependency the repositories could not serve — offline, or a
/// private one behind a VPN — which Gradle skips leniently and exits 0 on.
const GRADLE_INCOMPLETE: &str = "JAVELIN_INCOMPLETE=";

/// The build tool's process while one runs, for the owner of the session to
/// kill at shutdown: the thread that started it is never joined, and a process
/// exiting does not take its children with it.
pub type ChildSlot = std::sync::Arc<std::sync::Mutex<Option<u32>>>;

/// Kill the tool in `slot`, if one is running, with every process it started.
///
/// Killed under the slot's lock, which the runner holds while it reaps the
/// child: the id cannot be reaped and reused between being read and signalled.
pub fn kill_child(slot: &ChildSlot) {
    if let Ok(mut slot) = slot.lock()
        && let Some(pid) = slot.take()
    {
        kill_tree(pid);
    }
}

/// On Unix the tool leads a process group of its own, so the group is
/// killed; on Windows `taskkill /T` walks the tree.
fn kill_tree(pid: u32) {
    let mut command = if cfg!(windows) {
        let mut command = Command::new("taskkill");
        command.args(["/T", "/F", "/PID", &pid.to_string()]);
        command
    } else {
        let mut command = Command::new("kill");
        command.args(["-KILL", "--", &format!("-{pid}")]);
        command
    };
    let _ = command.stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// The workspace's dependency classpath: the JARs Maven or Gradle resolves,
/// in classpath order, each once, existing files only.
///
/// `cache` is where the answer is kept between sessions, keyed by
/// [`build_files_key`] — `None` keeps nothing and believes nothing kept. A
/// key that matches, naming only JARs that still exist, is the answer and no
/// process is started.
///
/// A run that fails still yields whatever it printed — one module of a
/// multi-module build failing does not take the others' classpaths with it —
/// and caches nothing, so the next session asks again; so does a run that
/// yields nothing at all. A cancelled run yields [`Error::Cancelled`].
pub fn resolve_classpath(
    root: &Path,
    kind: BuildSystemKind,
    cache: Option<&Path>,
    cancel: &CancellationToken,
    child: &ChildSlot,
) -> Result<Vec<PathBuf>> {
    resolve_classpath_reporting(root, kind, cache, cancel, child).map(|resolved| resolved.jars)
}

/// A classpath, and whether it is the whole of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub jars: Vec<PathBuf>,
    /// The tool succeeded, or its successful answer was cached. `false` for
    /// what a failed run printed, which is used and is not all there is.
    pub complete: bool,
    /// What each module compiles against, when the tool said; empty when it
    /// answered with one classpath for the whole build.
    pub modules: Vec<Module>,
}

/// One module of the build: the directories it claims, and its classpath.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    /// Where its sources are: a Gradle project's Java source directories, a
    /// Maven module's own directory.
    pub sources: Vec<PathBuf>,
    /// Its classpath, as indices into [`Resolved::jars`]; `None` for a module
    /// the tool did not answer for — not in the reactor, or packaging `pom`,
    /// which compiles nothing.
    pub jars: Option<Vec<usize>>,
}

/// A module as the tool printed it, before its entries are indexed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Answered {
    sources: Vec<PathBuf>,
    entries: Option<Vec<PathBuf>>,
}

/// Where each Maven module writes its classpath, relative to its own
/// directory — Maven aligns a relative `File` parameter to the module's
/// basedir, so a reactor of ten writes ten files. Named per run, so a file
/// another run left — or another session is writing — is never read.
const MAVEN_OUTPUT: &str = "target/javelin-classpath-";

/// What a cached answer is written in; part of the key, so an answer an
/// older format kept is a miss rather than a misreading.
const CACHE_FORMAT: &str = "modules-2";

/// [`resolve_classpath`], saying whether the answer is whole.
pub fn resolve_classpath_reporting(
    root: &Path,
    kind: BuildSystemKind,
    cache: Option<&Path>,
    cancel: &CancellationToken,
    child: &ChildSlot,
) -> Result<Resolved> {
    let key = build_files_key(root, kind);
    // ponytail: a build with a movable version, or a Gradle one whose cached
    // answer names a snapshot, asks the tool every session. The upgrade is
    // answering from the cache at once and re-asking behind it, the answer
    // whole when the two agree.
    if !movable_versions(root)
        && let Some((jars, modules)) = cache.and_then(|cache| read_cache(cache, &key))
        // Gradle keeps each snapshot it downloads in a directory named by its
        // hash, so the one named here may not be the one the build resolves
        // now; Maven keeps a snapshot at one path, which the JAR store
        // re-reads when it changes.
        && !(kind == BuildSystemKind::Gradle
            && jars
                .iter()
                .any(|jar| jar.to_string_lossy().contains("-SNAPSHOT")))
    {
        return Ok(Resolved {
            jars,
            complete: true,
            modules,
        });
    }
    let (succeeded, output, answered) = match kind {
        BuildSystemKind::Maven => {
            let mut command = Command::new(
                wrapper(root, "mvnw", "mvnw.cmd").unwrap_or(tool("mvn", "mvn.cmd").into()),
            );
            let output = format!(
                "{MAVEN_OUTPUT}{}-{}.txt",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since| since.as_nanos())
            );
            command.args([
                "-B",
                "--fail-at-end",
                "-Dstyle.color=never",
                "dependency:build-classpath",
                &format!("-Dmdep.outputFile={output}"),
                "-Dmdep.regenerateFile=true",
            ]);
            let ran = run(command, root, cancel, child, kind);
            // Collected whatever happened, so a cancelled run leaves no file.
            let answered = maven_modules(root, &output);
            let (succeeded, text) = ran?;
            let mut entries = parse_maven_output(&text);
            // With an output file the plugin prints no classpath, so a run
            // that left no file and printed none — a POM overriding
            // `outputFile` — answered nothing, and calling that whole would
            // flag every dependency.
            let answered_anything = !answered.is_empty() || !entries.is_empty();
            for module in &answered {
                entries.extend(module.entries.iter().flatten().cloned());
            }
            (succeeded && answered_anything, entries, answered)
        }
        BuildSystemKind::Gradle => {
            let script = match cache {
                Some(cache) => cache.with_file_name("javelin-classpath.gradle"),
                None => std::env::temp_dir()
                    .join(format!("javelin-classpath-{}.gradle", std::process::id())),
            };
            if let Some(parent) = script.parent() {
                std::fs::create_dir_all(parent).map_err(|source| Error::io(parent, source))?;
            }
            std::fs::write(&script, GRADLE_INIT).map_err(|source| Error::io(&script, source))?;
            let mut command = Command::new(
                wrapper(root, "gradlew", "gradlew.bat")
                    .unwrap_or(tool("gradle", "gradle.bat").into()),
            );
            command
                .args(GRADLE_ARGS)
                .arg("--init-script")
                .arg(&script)
                .arg("javelinClasspath");
            let ran = run(command, root, cancel, child, kind);
            if cache.is_none() {
                let _ = std::fs::remove_file(&script);
            }
            let (succeeded, text) = ran?;
            // A lenient skip is a failure for caching and for wholeness alike:
            // a cached partial answer would outlive the network coming back.
            let partial = text
                .lines()
                .any(|line| line.trim().starts_with(GRADLE_INCOMPLETE));
            (
                succeeded && !partial,
                parse_gradle_output(&text),
                gradle_modules(&text),
            )
        }
    };
    let (jars, whole) = usable(output, root);
    let modules = indexed(answered, &jars);
    let succeeded = succeeded && whole;
    if !succeeded {
        tracing::warn!(
            build = kind.name(),
            jars = jars.len(),
            "build: the tool failed; using what it printed, caching nothing"
        );
    } else if let Some(cache) = cache
        && !jars.is_empty()
    {
        write_cache(cache, &key, &jars, &modules);
    }
    if !succeeded && jars.is_empty() {
        return Err(Error::Build {
            system: kind.name().to_owned(),
            message: "the tool failed and printed no classpath".to_owned(),
        });
    }
    Ok(Resolved {
        jars,
        complete: succeeded,
        modules,
    })
}

/// Each module's printed entries as indices into `jars`: an entry that is
/// not there — a directory, a file that does not exist — is no JAR of it.
fn indexed(answered: Vec<Answered>, jars: &[PathBuf]) -> Vec<Module> {
    let at: std::collections::HashMap<&Path, usize> = jars
        .iter()
        .enumerate()
        .map(|(index, jar)| (jar.as_path(), index))
        .collect();
    answered
        .into_iter()
        .map(|module| Module {
            sources: module.sources,
            jars: module.entries.map(|entries| {
                let mut indices: Vec<usize> = entries
                    .iter()
                    .filter_map(|entry| at.get(entry.as_path()).copied())
                    .collect();
                indices.sort_unstable();
                indices.dedup();
                indices
            }),
        })
        .collect()
}

/// Every module under `root` — a directory holding a `pom.xml` — with the
/// classpath file this run wrote there as `output`, each file deleted once
/// read. A module with no file, or whose POM packages `pom`, was not
/// answered for. None written at all is a run that never got that far, or
/// a POM writing elsewhere: no modules.
fn maven_modules(root: &Path, output: &str) -> Vec<Answered> {
    let mut directories = Vec::new();
    pom_directories(root, &mut directories);
    let mut any = false;
    let mut modules = Vec::new();
    for directory in directories {
        let file = directory.join(output);
        let text = std::fs::read_to_string(&file).ok();
        let _ = std::fs::remove_file(&file);
        any |= text.is_some();
        let packages_pom = std::fs::read_to_string(directory.join("pom.xml")).is_ok_and(|pom| {
            pom.split_whitespace()
                .collect::<String>()
                .contains("<packaging>pom</packaging>")
        });
        modules.push(Answered {
            sources: vec![directory],
            entries: text
                .filter(|_| !packages_pom)
                // A module with no dependencies writes an empty file, which
                // splits into one empty path rather than none.
                .map(|text| {
                    std::env::split_paths(text.trim())
                        .filter(|entry| !entry.as_os_str().is_empty())
                        .collect()
                }),
        });
    }
    if !any {
        return Vec::new();
    }
    modules
}

/// Every directory under `dir` holding a `pom.xml`, pruned as the build
/// files' walk is.
fn pom_directories(dir: &Path, out: &mut Vec<PathBuf>) {
    if dir.join("pom.xml").is_file() {
        out.push(dir.to_path_buf());
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if entry.file_type().is_ok_and(|kind| kind.is_dir())
            && !name.starts_with('.')
            && !PRUNED.contains(&name.as_ref())
        {
            pom_directories(&entry.path(), out);
        }
    }
}

/// Each source set that printed a source directory, with what it compiles
/// against — nothing known where its classpath failed to resolve. Nothing
/// at all when an entry came untagged, which no script of these tags prints.
fn gradle_modules(stdout: &str) -> Vec<Answered> {
    let mut projects: std::collections::BTreeMap<&str, Answered> =
        std::collections::BTreeMap::new();
    let empty = || Answered {
        sources: Vec::new(),
        entries: Some(Vec::new()),
    };
    for line in stdout.lines().map(str::trim) {
        if let Some(set) = line.strip_prefix("JAVELIN_SET_FAILED=") {
            projects.entry(set).or_insert_with(empty).entries = None;
            continue;
        }
        let (source, rest) = match (
            line.strip_prefix("JAVELIN_SRC="),
            line.strip_prefix("JAVELIN_SET="),
        ) {
            (Some(rest), _) => (true, rest),
            (_, Some(rest)) => (false, rest),
            _ => continue,
        };
        let Some((set, path)) = rest.split_once('\t') else {
            return Vec::new();
        };
        let module = projects.entry(set).or_insert_with(empty);
        if source {
            module.sources.push(PathBuf::from(path));
        } else if let Some(entries) = &mut module.entries {
            entries.push(PathBuf::from(path));
        }
    }
    projects
        .into_values()
        .filter(|module| !module.sources.is_empty())
        .collect()
}

/// What a build's own files say besides its dependencies, read as text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildHints {
    /// An annotation processor is configured — Gradle's `annotationProcessor`
    /// or `kapt`, Maven's `annotationProcessorPaths` — and so types no source
    /// declares, from a processor path the classpath does not show.
    pub processors: bool,
    /// A plugin generates sources — OpenAPI, jOOQ, JAXB, protobuf and the
    /// like, or one that adds a source directory.
    pub codegen: bool,
    /// Every Java language level the build names: a release, a source or a
    /// target level, a toolchain.
    pub releases: Vec<u32>,
    /// A tool that adds members to types a source declares is in the build —
    /// Manifold, or AspectJ's compiler with its inter-type declarations — so
    /// a type's written members are not all of them. Not Lombok: what it
    /// writes is read off each type's own annotations, and a type it writes
    /// more into than is modelled says so itself.
    ///
    /// ponytail: a hand-written list, so a member generator not on it is
    /// invisible and the members it adds read as missing. AspectJ's weaver
    /// alone is not on it: it weaves at load time, after javac has checked
    /// every call. The upgrade is an entry per tool as one is met — no build
    /// tool reports that a plugin adds members.
    pub rewrites: bool,
}

/// Build plugins whose output is Java sources, by the artifact a POM names
/// or a Gradle plugin's id. Matched as a plugin and never as a dependency:
/// `jaxb-runtime` or `protobuf-java` on the classpath generates nothing.
///
/// ponytail: a hand-written list, so a code generator not on it is invisible
/// until its output exists. The upgrade is asking the tool for its source
/// roots.
const CODEGEN_ARTIFACTS: &[&str] = &[
    "openapi-generator-maven-plugin",
    "swagger-codegen-maven-plugin",
    "jooq-codegen-maven",
    "jaxb2-maven-plugin",
    "maven-jaxb2-plugin",
    "jaxb-maven-plugin",
    "cxf-codegen-plugin",
    "jaxws-maven-plugin",
    "protobuf-maven-plugin",
    "avro-maven-plugin",
    "antlr4-maven-plugin",
    "javacc-maven-plugin",
    "jsonschema2pojo-maven-plugin",
    "graphql-codegen-maven-plugin",
    "apt-maven-plugin",
];

/// Gradle plugin ids whose output is Java sources, matched only where a
/// plugin is applied: `id '…'`, `id("…")`, `plugin: '…'`, or a catalogue's
/// `id = "…"`.
const CODEGEN_PLUGINS: &[&str] = &[
    "org.openapi.generator",
    "org.hidetake.swagger.generator",
    "nu.studer.jooq",
    "com.github.bjornvester.xjc",
    "com.github.bjornvester.wsdl2java",
    "io.mateo.cxf-codegen",
    "com.google.protobuf",
    "com.github.davidmc24.gradle.plugin.avro",
    "antlr",
    "org.jsonschema2pojo",
    "io.github.kobylynskyi.graphql.codegen",
    "com.netflix.dgs.codegen",
];

/// Whether `text` applies a source generator as a plugin.
fn applies_codegen(text: &str) -> bool {
    CODEGEN_ARTIFACTS
        .iter()
        .any(|artifact| text.contains(&format!("<artifactId>{artifact}</artifactId>")))
        || CODEGEN_PLUGINS.iter().any(|id| {
            [
                format!("id '{id}'"),
                format!("id \"{id}\""),
                format!("id(\"{id}\")"),
                format!("id('{id}')"),
                format!("plugin: '{id}'"),
                format!("plugin: \"{id}\""),
                format!("id = \"{id}\""),
            ]
            .iter()
            .any(|written| text.contains(written.as_str()))
        })
}

/// What the build files under `root` say, besides the dependencies the tool
/// is asked for.
pub fn build_hints(root: &Path) -> BuildHints {
    let mut files = Vec::new();
    collect_build_files(root, &mut files);
    let mut hints = BuildHints::default();
    for (path, _, size) in files {
        if size > 1 << 20 {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        hints.processors |= text.contains("annotationProcessor") || text.contains("kapt");
        hints.codegen |= applies_codegen(&text);
        hints.rewrites |= ["manifold", "aspectj-maven-plugin", "freefair.aspectj"]
            .iter()
            .any(|tool| text.contains(tool));
        hints.releases.extend(java_levels(&text));
    }
    hints.releases.sort_unstable();
    hints.releases.dedup();
    hints
}

/// Whether a build file names a version that can resolve to another JAR
/// with no build file moving — a range, a dynamic version, Maven's `LATEST`
/// and `RELEASE` — while the JAR it resolved to before is still there for a
/// cached answer to name. Over-inclusive on purpose: a false match costs one
/// run of the tool.
///
/// ponytail: a remote `SNAPSHOT` parent or BOM can move a dependency's
/// version too, and is not seen, because matching `SNAPSHOT` would match
/// nearly every project's own version. The upgrade is reading `<parent>`
/// and import-scoped dependencies.
fn movable_versions(root: &Path) -> bool {
    const MARKERS: &[&str] = &[
        "LATEST<", "RELEASE<", "latest.", ">[", ">(", "+'", "+\"", "'[", "\"[", "'(", "\"(",
    ];
    let mut files = Vec::new();
    collect_build_files(root, &mut files);
    files.into_iter().any(|(path, _, size)| {
        // Unread is unknown, which is movable.
        size > 1 << 20
            || std::fs::read(&path).map_or(true, |bytes| {
                let text = String::from_utf8_lossy(&bytes);
                MARKERS.iter().any(|marker| text.contains(marker))
            })
    })
}

/// Every Java language level `text` names, as a feature number: `1.8` and
/// `VERSION_1_8` are 8. A property reference or anything else that is not a
/// number is not a level.
fn java_levels(text: &str) -> Vec<u32> {
    const AFTER: &[&str] = &[
        "<maven.compiler.release>",
        "<maven.compiler.source>",
        "<maven.compiler.target>",
        "<java.version>",
        "<release>",
        "<source>",
        "<target>",
        "JavaLanguageVersion.of(",
        "JavaVersion.VERSION_",
        "sourceCompatibility",
        "targetCompatibility",
        "release.set(",
        "options.release",
        "jvmToolchain(",
    ];
    let mut levels = Vec::new();
    for marker in AFTER {
        for (at, _) in text.match_indices(marker) {
            let rest = text[at + marker.len()..].trim_start_matches(|ch: char| {
                ch.is_whitespace() || matches!(ch, '=' | '\'' | '"')
            });
            let value: String = rest
                .chars()
                .take_while(|ch| ch.is_ascii_digit() || matches!(ch, '.' | '_'))
                .collect();
            let mut parts = value.split(['.', '_']).filter(|part| !part.is_empty());
            let level = match parts.next() {
                Some("1") => parts.next(),
                first => first,
            };
            if let Some(level) = level.and_then(|level| level.parse::<u32>().ok())
                && (1..100).contains(&level)
            {
                levels.push(level);
            }
        }
    }
    levels
}

/// Whether any build file is anywhere under `root` — not only at it, the
/// check [`crate::detect`] makes: a folder holding a Maven module one level
/// down has dependencies all the same.
pub fn has_build_files(root: &Path) -> bool {
    let mut files = Vec::new();
    collect_build_files(root, &mut files);
    !files.is_empty()
}

/// The tool on `PATH`: a batch file on Windows, which `Command` does not find
/// by the bare name.
pub(crate) fn tool(unix: &'static str, windows: &'static str) -> &'static str {
    if cfg!(windows) { windows } else { unix }
}

/// The project's own wrapper script, when it has one.
pub(crate) fn wrapper(root: &Path, unix: &str, windows: &str) -> Option<PathBuf> {
    let name = if cfg!(windows) { windows } else { unix };
    let path = root.join(name);
    path.is_file().then_some(path)
}

/// Run `command` in `root` to completion and hand back its standard output,
/// killing it on cancellation or after [`TIMEOUT`].
///
/// The whole process tree is killed, because the child is usually a wrapper
/// script and the JVM it started would otherwise run to its own end. The
/// child's id sits in `slot` while it runs, and the slot is only cleared
/// under its lock in the same step as the child is reaped, so a kill from the
/// session never signals a reused id.
///
/// Answers whether the tool succeeded, beside what it printed.
///
/// ponytail: its own process group takes the tool out of the editor's
/// group, so a server that is killed outright — not shut down — leaves it
/// running to its own end or the network's. The upgrade is a watchdog on the
/// tool's side of the pipe.
pub(crate) fn run(
    mut command: Command,
    root: &Path,
    cancel: &CancellationToken,
    slot: &ChildSlot,
    kind: BuildSystemKind,
) -> Result<(bool, String)> {
    let failed = |message: String| Error::Build {
        system: kind.name().to_owned(),
        message,
    };
    cancel.check().map_err(|_| Error::Cancelled)?;
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| failed(format!("could not start: {err}")))?;
    if let Ok(mut slot) = slot.lock() {
        *slot = Some(child.id());
    }
    // A session dropped between the spawn and the line above killed nothing:
    // it cancels before it reads the slot, so this sees it.
    if cancel.is_cancelled() {
        kill_child(slot);
    }
    // Drained on a thread of its own: a child blocked on a full pipe never
    // exits, and polling for exit would wait for the timeout.
    let mut stdout = child.stdout.take().expect("piped above");
    // Bytes, not a string: Maven on a Windows console code page, or a home
    // directory named in anything but UTF-8, would otherwise read as nothing.
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    });
    let started = Instant::now();
    let status = loop {
        let waited = {
            let mut guard = slot.lock().ok();
            let waited = child.try_wait();
            if !matches!(waited, Ok(None))
                && let Some(guard) = guard.as_mut()
            {
                **guard = None;
            }
            waited
        };
        if let Some(status) = waited.map_err(|err| failed(format!("could not wait: {err}")))? {
            break status;
        }
        if cancel.is_cancelled() || started.elapsed() > TIMEOUT {
            kill_child(slot);
            let _ = child.kill();
            let _ = child.wait();
            // Not joined: a descendant that escaped the group can hold the
            // pipe open for as long as it likes, and the reader ends with it.
            drop(reader);
            return Err(if cancel.is_cancelled() {
                Error::Cancelled
            } else {
                failed("timed out".to_owned())
            });
        }
        std::thread::sleep(POLL);
    };
    let text = reader.join().unwrap_or_default();
    // Killed by the session between two polls: the exit is the kill's.
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if !status.success() {
        tracing::debug!(build = kind.name(), %status, "build: the tool failed");
    }
    Ok((status.success(), text))
}

/// The classpath entries Maven's `dependency:build-classpath` logs, one
/// module after another: the line after each `Dependencies classpath:`.
pub fn parse_maven_output(stdout: &str) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    let mut lines = stdout.lines();
    while let Some(line) = lines.next() {
        if !line.trim_end().ends_with("Dependencies classpath:") {
            continue;
        }
        let Some(classpath) = lines.next() else {
            break;
        };
        let classpath = classpath.trim().trim_start_matches("[INFO]").trim();
        if !classpath.is_empty() {
            entries.extend(std::env::split_paths(classpath));
        }
    }
    entries
}

/// The entries the init script prints, each prefixed `JAVELIN_CP=` — or
/// `JAVELIN_SET=`, a source set's, which may reach past every configuration.
pub fn parse_gradle_output(stdout: &str) -> Vec<PathBuf> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            line.strip_prefix("JAVELIN_CP=")
                .or_else(|| line.strip_prefix("JAVELIN_SET="))
        })
        // `<project path>\t<file>`, or a file alone from an untagged script.
        .map(|rest| PathBuf::from(rest.split_once('\t').map_or(rest, |(_, file)| file)))
        .collect()
}

/// Existing files, each once, in order. A directory is a sibling module's
/// compiled output, whose sources the workspace walk already indexes.
///
/// And whether that is all of it: an entry dropped from outside `root` — a
/// sibling project's JAR not built yet, a class directory of an included
/// build — names types no walk indexes, so the answer is not whole. One under
/// `root` is a module whose sources the walk takes.
///
/// ponytail: a class directory from outside the workspace is dropped rather
/// than read. The upgrade is reading its class files as a JAR's are read.
fn usable(entries: Vec<PathBuf>, root: &Path) -> (Vec<PathBuf>, bool) {
    let mut seen = HashSet::new();
    let mut whole = true;
    let mut jars = Vec::new();
    for entry in entries {
        if entry.is_file() {
            if seen.insert(entry.clone()) {
                jars.push(entry);
            }
        } else if !entry.starts_with(root) {
            whole = false;
        }
    }
    (jars, whole)
}

/// A hash of the tool and of every build file under `root` — its path,
/// modification time and size — which is what a cached classpath is keyed
/// by. Build output and dot-directories other than `.mvn/` are not walked;
/// no symlink is followed.
pub fn build_files_key(root: &Path, kind: BuildSystemKind) -> String {
    let mut files = Vec::new();
    collect_build_files(root, &mut files);
    files.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    kind.name().hash(&mut hasher);
    // A cached answer is only as good as what asked for it: a changed init
    // script asks for more, so an answer the old one gave is a miss — and
    // one read through a rich console may have lost lines and been kept.
    GRADLE_INIT.hash(&mut hasher);
    GRADLE_ARGS.hash(&mut hasher);
    CACHE_FORMAT.hash(&mut hasher);
    MAVEN_OUTPUT.hash(&mut hasher);
    for (path, modified, size) in files {
        path.hash(&mut hasher);
        modified.hash(&mut hasher);
        size.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

fn collect_build_files(dir: &Path, out: &mut Vec<(PathBuf, u128, u64)>) {
    collect_under(dir, false, out);
}

/// `all` is true under `.mvn/`, `buildSrc/` and `build-logic/`.
fn collect_under(dir: &Path, all: bool, out: &mut Vec<(PathBuf, u128, u64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if kind.is_dir() {
            // `.mvn/` is Maven's configuration, and `buildSrc/` and
            // `build-logic/` are code that pins versions — `Deps.kt` — so
            // every file under them counts.
            if name == ".mvn" || name == "buildSrc" || name == "build-logic" {
                collect_under(&entry.path(), true, out);
            } else if !name.starts_with('.') && !PRUNED.contains(&name.as_ref()) {
                collect_under(&entry.path(), all, out);
            }
        } else if kind.is_file() && !name.ends_with(".jar") && (all || is_build_file(&name)) {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |since| since.as_nanos());
            out.push((entry.path(), modified, metadata.len()));
        }
    }
}

/// The cached classpath, if its key is `key` and every JAR it names still
/// exists: one that is gone means the cache predates something — a purged
/// repository, a version bump — and is a miss rather than a shorter list.
fn read_cache(cache: &Path, key: &str) -> Option<(Vec<PathBuf>, Vec<Module>)> {
    let text = std::fs::read_to_string(cache).ok()?;
    let mut lines = text.lines();
    if lines.next()? != key {
        return None;
    }
    let mut jars = Vec::new();
    let mut modules = Vec::new();
    for line in lines {
        match line.strip_prefix(MODULE_LINE) {
            Some(module) => modules.push(read_module(module)?),
            None => jars.push(PathBuf::from(line)),
        }
    }
    let indices_valid = modules
        .iter()
        .flat_map(|module| module.jars.iter().flatten())
        .all(|index| *index < jars.len());
    (indices_valid && jars.iter().all(|jar| jar.is_file())).then_some((jars, modules))
}

/// A cached module line: `@module`, its JARs' indices comma-separated or
/// `-` for none answered, then its source directories, tab-separated.
const MODULE_LINE: &str = "@module\t";

fn read_module(line: &str) -> Option<Module> {
    let mut fields = line.split('\t');
    let jars = match fields.next()? {
        "-" => None,
        "" => Some(Vec::new()),
        indices => Some(
            indices
                .split(',')
                .map(|index| index.parse().ok())
                .collect::<Option<Vec<usize>>>()?,
        ),
    };
    Some(Module {
        sources: fields.map(PathBuf::from).collect(),
        jars,
    })
}

/// A cache that cannot be written costs the next session a run of the tool
/// and this one nothing.
fn write_cache(cache: &Path, key: &str, jars: &[PathBuf], modules: &[Module]) {
    let mut text = format!("{key}\n");
    for jar in jars {
        text.push_str(&jar.to_string_lossy());
        text.push('\n');
    }
    for module in modules {
        text.push_str(MODULE_LINE);
        match &module.jars {
            None => text.push('-'),
            Some(indices) => text.push_str(
                &indices
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        }
        for source in &module.sources {
            text.push('\t');
            text.push_str(&source.to_string_lossy());
        }
        text.push('\n');
    }
    if let Some(parent) = cache.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(err) = std::fs::write(cache, text) {
        tracing::debug!(cache = %cache.display(), %err, "build: classpath not cached");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("javelin-build-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn maven_output_yields_every_modules_classpath_in_order() {
        let separator = if cfg!(windows) { ";" } else { ":" };
        let output = format!(
            "[INFO] Scanning for projects...\n\
             [INFO] --- dependency:3.6.1:build-classpath (default-cli) @ core ---\n\
             [INFO] Dependencies classpath:\n\
             [INFO] /m2/a.jar{separator}/m2/b.jar\n\
             [INFO] --- dependency:3.6.1:build-classpath (default-cli) @ empty ---\n\
             [INFO] Dependencies classpath:\n\
             [INFO] \n\
             [INFO] --- dependency:3.6.1:build-classpath (default-cli) @ web ---\n\
             [INFO] Dependencies classpath:\n\
             /m2/c.jar\n\
             [INFO] BUILD SUCCESS\n"
        );
        assert_eq!(
            parse_maven_output(&output),
            vec![
                PathBuf::from("/m2/a.jar"),
                PathBuf::from("/m2/b.jar"),
                PathBuf::from("/m2/c.jar")
            ]
        );
    }

    /// Manifold and AspectJ add members to types a source declares, which no
    /// member list read from that source can show. Lombok is answered per
    /// type.
    #[test]
    fn a_build_using_a_tool_that_rewrites_types_says_so() {
        let dir = std::env::temp_dir().join(format!("javelin-rewrites-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        for (file, text, rewrites) in [
            ("pom.xml", "<artifactId>lombok</artifactId>", false),
            (
                "pom.xml",
                "<artifactId>aspectj-maven-plugin</artifactId>",
                true,
            ),
            ("build.gradle", "plugins { id 'io.freefair.aspectj' }", true),
            (
                "build.gradle",
                "dependencies { implementation 'systems.manifold:manifold-ext' }",
                true,
            ),
            ("pom.xml", "<artifactId>aspectjweaver</artifactId>", false),
            ("pom.xml", "<artifactId>guava</artifactId>", false),
        ] {
            let _ = std::fs::remove_file(dir.join("pom.xml"));
            let _ = std::fs::remove_file(dir.join("build.gradle"));
            std::fs::write(dir.join(file), text).expect("build file");
            assert_eq!(build_hints(&dir).rewrites, rewrites, "{text}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_build_names_its_java_level_and_its_processors_and_generators() {
        let dir = std::env::temp_dir().join(format!("javelin-hints-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("app")).expect("dirs");
        std::fs::write(
            dir.join("pom.xml"),
            "<properties><maven.compiler.release>17</maven.compiler.release>\
             <java.version>${x}</java.version></properties>\
             <dependency><artifactId>jaxb-runtime</artifactId></dependency>\
             <plugin><artifactId>openapi-generator-maven-plugin</artifactId></plugin>",
        )
        .expect("pom");
        std::fs::write(
            dir.join("app/build.gradle"),
            "java { sourceCompatibility = JavaVersion.VERSION_1_8 }\n\
             dependencies { annotationProcessor 'x:y:1' }\n",
        )
        .expect("gradle");
        let hints = build_hints(&dir);
        assert_eq!(hints.releases, vec![8, 17]);
        assert!(hints.processors);
        assert!(hints.codegen);
        std::fs::write(
            dir.join("pom.xml"),
            "<dependency><artifactId>jaxb-runtime</artifactId></dependency>\
             <dependency><artifactId>protobuf-java</artifactId></dependency>\
             <dependency><artifactId>grpc-protobuf</artifactId></dependency>",
        )
        .expect("pom");
        assert!(
            !build_hints(&dir).codegen,
            "a generator's runtime on the classpath generates nothing"
        );
        assert!(applies_codegen(
            "plugins {\n    id(\"com.google.protobuf\") version \"0.9\"\n}"
        ));
        assert!(!applies_codegen(
            "implementation(\"com.google.protobuf:protobuf-java:4\")"
        ));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            java_levels("languageVersion = JavaLanguageVersion.of(21)\n<release>11</release>"),
            vec![11, 21]
        );
    }

    #[test]
    fn gradle_output_yields_the_prefixed_lines_only() {
        let output =
            "> Task :javelinClasspath\nJAVELIN_CP=/g/a.jar\nnoise\n  JAVELIN_CP=/g/b.jar\n";
        assert_eq!(
            parse_gradle_output(output),
            vec![PathBuf::from("/g/a.jar"), PathBuf::from("/g/b.jar")]
        );
    }

    /// Two projects' lines interleaved, as `org.gradle.parallel` prints them:
    /// each entry goes to the project its tag names, and a project that
    /// printed no source directory — a platform, the root of a build —
    /// claims nothing.
    #[test]
    fn gradle_output_is_read_per_project_by_its_tags() {
        let output = "JAVELIN_SRC=:a/main\t/w/a/src/main/java\n\
                      JAVELIN_SET=:a/main\t/g/guava.jar\n\
                      JAVELIN_SRC=:a/test\t/w/a/src/test/java\n\
                      JAVELIN_SET=:a/test\t/g/guava.jar\n\
                      JAVELIN_SET=:a/test\t/g/postgres.jar\n\
                      JAVELIN_CP=:a\t/g/guava.jar\n\
                      JAVELIN_SRC=:b/main\t/w/b/src/main/java\n\
                      JAVELIN_SET_FAILED=:b/main\n\
                      JAVELIN_SRC=:c/main\t/w/c/src/main/java\n\
                      JAVELIN_SET=:d/main\t/g/orphan.jar\n";
        assert_eq!(
            parse_gradle_output(output),
            [
                "/g/guava.jar",
                "/g/guava.jar",
                "/g/postgres.jar",
                "/g/guava.jar",
                "/g/orphan.jar"
            ]
            .map(PathBuf::from)
            .to_vec(),
            "a source set's classpath reaches past the configurations"
        );
        let module = |source: &str, entries: Option<&[&str]>| Answered {
            sources: vec![PathBuf::from(source)],
            entries: entries.map(|entries| entries.iter().map(PathBuf::from).collect()),
        };
        assert_eq!(
            gradle_modules(output),
            vec![
                module("/w/a/src/main/java", Some(&["/g/guava.jar"])),
                module(
                    "/w/a/src/test/java",
                    Some(&["/g/guava.jar", "/g/postgres.jar"])
                ),
                module("/w/b/src/main/java", None),
                module("/w/c/src/main/java", Some(&[])),
            ]
        );
        // An untagged entry says nothing about which source set it is for.
        assert!(gradle_modules("JAVELIN_SRC=:a\t/w/a\nJAVELIN_SET=/g/a.jar\n").is_empty());
    }

    #[test]
    fn the_key_moves_with_a_build_file_and_not_with_build_output() {
        let root = scratch("key");
        std::fs::write(root.join("pom.xml"), "<project/>").expect("pom");
        std::fs::create_dir_all(root.join("target")).expect("target");
        let key = build_files_key(&root, BuildSystemKind::Maven);
        std::fs::write(root.join("target/pom.xml"), "copied").expect("copy");
        std::fs::write(root.join("Main.java"), "class Main {}").expect("source");
        assert_eq!(build_files_key(&root, BuildSystemKind::Maven), key);
        std::fs::create_dir_all(root.join("module")).expect("module");
        std::fs::write(root.join("module/pom.xml"), "<project/>").expect("module pom");
        assert_ne!(build_files_key(&root, BuildSystemKind::Maven), key);
        assert_ne!(
            build_files_key(&root, BuildSystemKind::Maven),
            build_files_key(&root, BuildSystemKind::Gradle)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_entry_a_directory_and_a_repeat_are_dropped() {
        let root = scratch("usable");
        let jar = root.join("a.jar");
        std::fs::write(&jar, "zip").expect("jar");
        assert_eq!(
            usable(
                vec![
                    jar.clone(),
                    root.join("missing.jar"),
                    root.clone(),
                    jar.clone()
                ],
                &root
            ),
            (vec![jar.clone()], true),
            "a module under the root is walked"
        );
        let elsewhere = root.with_file_name("javelin-elsewhere-lib.jar");
        assert_eq!(
            usable(vec![jar.clone(), elsewhere], &root),
            (vec![jar], false),
            "a sibling project's JAR not built yet is a hole"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    /// A fake `mvnw`'s first lines: `$out` is the output file it was asked
    /// to write, relative to each module's directory.
    const WRITES_OUTPUT: &str = "out=\nfor arg; do case \"$arg\" in \
                                 -Dmdep.outputFile=*) out=\"${arg#-Dmdep.outputFile=}\";; \
                                 esac; done\n";

    #[cfg(unix)]
    fn fake_mvnw(root: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        let path = root.join("mvnw");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("mvnw");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    /// A reactor whose modules each write their own classpath: every module
    /// gets its own JARs, a module the run did not answer for and a `pom`
    /// module get none, every file is deleted, and a second session reads
    /// the same modules back out of the cache without running anything.
    #[cfg(unix)]
    #[test]
    fn each_maven_module_gets_the_classpath_it_wrote() {
        let root = scratch("maven-modules");
        let jar = |name: &str| {
            let jar = root.join(format!("repo/{name}.jar"));
            std::fs::create_dir_all(jar.parent().expect("repo")).expect("repo");
            std::fs::write(&jar, "zip").expect("jar");
            jar
        };
        let (guava, mockito) = (jar("guava"), jar("mockito"));
        std::fs::write(
            root.join("pom.xml"),
            "<project>\n  <packaging>\n    pom\n  </packaging>\n</project>",
        )
        .expect("pom");
        for module in ["core", "web", "lone", "tools/standalone"] {
            std::fs::create_dir_all(root.join(module)).expect("module");
            std::fs::write(root.join(module).join("pom.xml"), "<project/>").expect("pom");
        }
        let separator = if cfg!(windows) { ";" } else { ":" };
        fake_mvnw(
            &root,
            &format!(
                "{WRITES_OUTPUT}\
                 mkdir -p target core/target web/target lone/target\n\
                 printf '' > \"$out\"\n\
                 printf '' > \"lone/$out\"\n\
                 printf '{}' > \"core/$out\"\n\
                 printf '{}{separator}{}' > \"web/$out\"",
                guava.display(),
                guava.display(),
                mockito.display()
            ),
        );
        let cache = root.join("cache/classpath.txt");
        let resolve = || {
            resolve_classpath_reporting(
                &root,
                BuildSystemKind::Maven,
                Some(&cache),
                &CancellationToken::new(),
                &ChildSlot::default(),
            )
            .expect("resolved")
        };
        let resolved = resolve();
        assert!(resolved.complete);
        assert_eq!(resolved.jars, vec![guava.clone(), mockito.clone()]);
        let jars_of = |resolved: &Resolved, module: &str| {
            resolved
                .modules
                .iter()
                .find(|it| it.sources == vec![root.join(module)])
                .unwrap_or_else(|| panic!("{module}: {:?}", resolved.modules))
                .jars
                .clone()
        };
        let expect = |resolved: &Resolved| {
            assert_eq!(resolved.modules.len(), 5, "{:?}", resolved.modules);
            assert_eq!(jars_of(resolved, ""), None, "a pom module compiles nothing");
            assert_eq!(jars_of(resolved, "core"), Some(vec![0]));
            assert_eq!(jars_of(resolved, "web"), Some(vec![0, 1]));
            assert_eq!(jars_of(resolved, "lone"), Some(vec![]), "no dependencies");
            assert_eq!(
                jars_of(resolved, "tools/standalone"),
                None,
                "not in the reactor"
            );
        };
        expect(&resolved);
        for module in ["", "core", "web", "lone"] {
            let left: Vec<_> = std::fs::read_dir(root.join(module).join("target"))
                .expect("target")
                .flatten()
                .map(|entry| entry.file_name())
                .collect();
            assert!(left.is_empty(), "{module}: {left:?}");
        }
        fake_mvnw(&root, "exit 1");
        expect(&resolve());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// No module wrote a file — an older plugin, a run that failed early —
    /// and stdout's one classpath is the answer, with no modules to narrow it.
    #[cfg(unix)]
    #[test]
    fn a_maven_run_that_wrote_no_module_file_is_one_classpath() {
        let root = scratch("maven-no-modules");
        let jar = root.join("a.jar");
        std::fs::write(&jar, "zip").expect("jar");
        std::fs::write(root.join("pom.xml"), "<project/>").expect("pom");
        std::fs::create_dir_all(root.join("target")).expect("target");
        // Left by an earlier run, or being written by another session: not
        // this one's answer.
        let stale = root.join(format!("{MAVEN_OUTPUT}1-2.txt"));
        std::fs::write(&stale, "/elsewhere/other.jar").expect("stale");
        fake_mvnw(
            &root,
            &format!(
                "echo '[INFO] Dependencies classpath:'\necho '[INFO] {}'",
                jar.display()
            ),
        );
        let resolved = resolve_classpath_reporting(
            &root,
            BuildSystemKind::Maven,
            None,
            &CancellationToken::new(),
            &ChildSlot::default(),
        )
        .expect("resolved");
        assert_eq!(resolved.jars, vec![jar]);
        assert!(resolved.complete);
        assert!(resolved.modules.is_empty(), "{:?}", resolved.modules);
        assert!(
            stale.exists(),
            "another run's file is not this run's to delete"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A run that succeeds and answers nowhere — a POM writing the file
    /// somewhere of its own — is not a build with no dependencies.
    #[cfg(unix)]
    #[test]
    fn a_maven_run_that_answered_nowhere_is_not_whole() {
        let root = scratch("maven-nowhere");
        std::fs::write(root.join("pom.xml"), "<project/>").expect("pom");
        fake_mvnw(&root, "echo '[INFO] Wrote classpath file elsewhere'");
        let resolved = resolve_classpath_reporting(
            &root,
            BuildSystemKind::Maven,
            None,
            &CancellationToken::new(),
            &ChildSlot::default(),
        );
        assert!(
            resolved.as_ref().is_err() || resolved.as_ref().is_ok_and(|it| !it.complete),
            "{resolved:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    /// A version range or a dynamic version can resolve to another JAR with
    /// no build file moving, so a cached answer for one is never believed —
    /// the old JAR is still there to name. A project's own `SNAPSHOT`
    /// version is not one: Maven keeps a snapshot at one path.
    #[test]
    fn a_build_whose_versions_can_move_is_asked_every_time() {
        for (name, build, cached) in [
            (
                "range",
                "<project><version>[1.0,2.0)</version></project>",
                false,
            ),
            (
                "latest",
                "<project><version>LATEST</version></project>",
                false,
            ),
            (
                "gradle-plus",
                "dependencies { implementation 'a:b:1.+' }",
                false,
            ),
            (
                "gradle-latest",
                "dependencies { implementation(\"a:b:latest.release\") }",
                false,
            ),
            (
                "snapshot",
                "<project><version>1.0-SNAPSHOT</version></project>",
                true,
            ),
        ] {
            let root = scratch(&format!("volatile-{name}"));
            // At one path however often Maven refreshes it.
            let jar = root.join("lib-1.0-SNAPSHOT.jar");
            std::fs::write(&jar, "zip").expect("jar");
            std::fs::write(root.join("pom.xml"), build).expect("build file");
            fake_mvnw(
                &root,
                &format!(
                    "echo '[INFO] Dependencies classpath:'\necho '[INFO] {}'",
                    jar.display()
                ),
            );
            let cache = root.join("cache/classpath.txt");
            let resolve = || {
                resolve_classpath_reporting(
                    &root,
                    BuildSystemKind::Maven,
                    Some(&cache),
                    &CancellationToken::new(),
                    &ChildSlot::default(),
                )
            };
            assert!(resolve().expect("resolved").complete, "{name}");
            fake_mvnw(&root, "exit 1");
            assert_eq!(resolve().is_ok(), cached, "{name}");
        }
    }

    /// Gradle keeps each snapshot it downloads in a directory of its own
    /// hash, so the one a cached answer names may no longer be the one the
    /// build resolves.
    #[cfg(unix)]
    #[test]
    fn a_cached_gradle_answer_naming_a_snapshot_is_asked_again() {
        use std::os::unix::fs::PermissionsExt as _;
        for (name, cached) in [("lib-1.0-SNAPSHOT.jar", false), ("lib-1.0.jar", true)] {
            let root = scratch(&format!("gradle-snapshot-{cached}"));
            let jar = root.join(name);
            std::fs::write(&jar, "zip").expect("jar");
            std::fs::write(root.join("build.gradle"), "").expect("build");
            let gradlew = root.join("gradlew");
            let write = |body: String| {
                std::fs::write(&gradlew, format!("#!/bin/sh\n{body}\n")).expect("gradlew");
                std::fs::set_permissions(&gradlew, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            };
            write(format!("echo 'JAVELIN_CP={}'", jar.display()));
            // Outside the root: the init script beside it is a Gradle script.
            let cache = scratch(&format!("gradle-snapshot-cache-{cached}")).join("classpath.txt");
            let resolve = || {
                resolve_classpath_reporting(
                    &root,
                    BuildSystemKind::Gradle,
                    Some(&cache),
                    &CancellationToken::new(),
                    &ChildSlot::default(),
                )
            };
            assert!(resolve().expect("resolved").complete, "{name}");
            write("exit 1".to_owned());
            assert_eq!(resolve().is_ok(), cached, "{name}");
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_wrapper_is_run_once_and_its_answer_cached_until_a_build_file_moves() {
        let root = scratch("wrapper");
        let jar = root.join("lib.jar");
        std::fs::write(&jar, "zip").expect("jar");
        std::fs::write(root.join("pom.xml"), "<project/>").expect("pom");
        fake_mvnw(
            &root,
            &format!(
                "echo '[INFO] Dependencies classpath:'\necho '[INFO] {}'",
                jar.display()
            ),
        );
        let cache = root.join("cache/classpath.txt");
        let cancel = CancellationToken::new();
        assert_eq!(
            resolve_classpath(
                &root,
                BuildSystemKind::Maven,
                Some(&cache),
                &cancel,
                &ChildSlot::default()
            )
            .expect("resolved"),
            vec![jar.clone()]
        );
        // A wrapper that would fail: the cached answer means it is not run.
        fake_mvnw(&root, "exit 1");
        assert_eq!(
            resolve_classpath(
                &root,
                BuildSystemKind::Maven,
                Some(&cache),
                &cancel,
                &ChildSlot::default()
            )
            .expect("cached"),
            vec![jar]
        );
        std::fs::write(root.join("pom.xml"), "<project><changed/></project>").expect("pom");
        assert!(
            resolve_classpath(
                &root,
                BuildSystemKind::Maven,
                Some(&cache),
                &cancel,
                &ChildSlot::default()
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_cancelled_run_is_killed_promptly_and_caches_nothing() {
        let root = scratch("cancelled");
        std::fs::write(root.join("pom.xml"), "<project/>").expect("pom");
        let pid = root.join("sleep.pid");
        fake_mvnw(
            &root,
            &format!("sleep 30 &\necho $! > '{}'\nwait", pid.display()),
        );
        let cache = root.join("cache/classpath.txt");
        let cancel = CancellationToken::new();
        // Cancelled once the tool is demonstrably running, not after a guess
        // at how long a process takes to start.
        let canceller = {
            let cancel = cancel.clone();
            let pid = pid.clone();
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !pid.exists() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                cancel.cancel();
            })
        };
        let started = Instant::now();
        let result = resolve_classpath(
            &root,
            BuildSystemKind::Maven,
            Some(&cache),
            &cancel,
            &ChildSlot::default(),
        );
        let _ = canceller.join();
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(!cache.exists());
        // The wrapper's own child, not only the wrapper, is gone.
        let pid = std::fs::read_to_string(&pid).expect("the sleep started");
        let alive = || {
            Command::new("kill")
                .args(["-0", pid.trim()])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while alive() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(), "the grandchild outlived the cancellation");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Where dependencies are declared besides the obvious files: Maven's
    /// `.mvn/`, a Gradle convention plugin in `buildSrc`, an applied script,
    /// a custom version catalogue, the wrapper's own version.
    #[test]
    fn the_key_moves_with_every_file_that_can_declare_a_dependency() {
        let root = scratch("key-wide");
        std::fs::write(root.join("pom.xml"), "<project/>").expect("pom");
        for relative in [
            ".mvn/maven.config",
            ".mvn/wrapper/maven-wrapper.properties",
            "buildSrc/src/main/kotlin/conventions.gradle.kts",
            "gradle/dependencies.gradle",
            "gradle/custom.versions.toml",
            "gradle/wrapper/gradle-wrapper.properties",
            "buildSrc/src/main/kotlin/Deps.kt",
            "build-logic/src/main/kotlin/Versions.kt",
        ] {
            let key = build_files_key(&root, BuildSystemKind::Maven);
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("dirs");
            std::fs::write(&path, "x").expect("write");
            assert_ne!(
                build_files_key(&root, BuildSystemKind::Maven),
                key,
                "{relative}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    fn resolve(root: &Path, cache: &Path) -> Result<Vec<PathBuf>> {
        resolve_classpath(
            root,
            BuildSystemKind::Maven,
            Some(cache),
            &CancellationToken::new(),
            &ChildSlot::default(),
        )
    }

    /// A cached JAR that is gone — a purged `~/.m2`, a version bump the
    /// cache predates — makes the cache a miss rather than a shorter list.
    #[cfg(unix)]
    #[test]
    fn a_cached_jar_that_is_gone_runs_the_tool_again() {
        let root = scratch("vanished");
        let jar = root.join("lib.jar");
        std::fs::write(&jar, "zip").expect("jar");
        std::fs::write(root.join("pom.xml"), "<project/>").expect("pom");
        fake_mvnw(
            &root,
            &format!(
                "echo '[INFO] Dependencies classpath:'\necho '{}'",
                jar.display()
            ),
        );
        let cache = root.join("cache/classpath.txt");
        resolve(&root, &cache).expect("resolved");
        std::fs::remove_file(&jar).expect("purged");
        fake_mvnw(&root, "exit 1");
        assert!(resolve(&root, &cache).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A multi-module build where one module fails still prints the others'
    /// classpaths: they are used, and not cached, so the next session tries
    /// again.
    #[cfg(unix)]
    #[test]
    fn a_failed_run_still_yields_what_it_printed_and_caches_nothing() {
        let root = scratch("partial");
        let jar = root.join("lib.jar");
        std::fs::write(&jar, "zip").expect("jar");
        std::fs::write(root.join("pom.xml"), "<project/>").expect("pom");
        fake_mvnw(
            &root,
            &format!(
                "echo '[INFO] Dependencies classpath:'\necho '{}'\nexit 1",
                jar.display()
            ),
        );
        let cache = root.join("cache/classpath.txt");
        assert_eq!(resolve(&root, &cache).expect("partial"), vec![jar]);
        assert!(!cache.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An answer with nothing in it — output in an encoding the parse did not
    /// expect, a tool that printed nothing — is not remembered as the answer.
    #[cfg(unix)]
    #[test]
    fn an_empty_answer_is_not_cached() {
        let root = scratch("empty");
        std::fs::write(root.join("pom.xml"), "<project/>").expect("pom");
        fake_mvnw(&root, "printf '\\377\\376 not utf-8'");
        let cache = root.join("cache/classpath.txt");
        assert_eq!(
            resolve(&root, &cache).unwrap_or_default(),
            Vec::<PathBuf>::new()
        );
        assert!(!cache.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Whether the answer is the whole classpath: a run that failed partway
    /// is used but is not whole, and diagnostics must know the difference.
    #[cfg(unix)]
    #[test]
    fn a_partial_answer_says_it_is_not_whole_and_a_cached_one_that_it_is() {
        let root = scratch("reporting");
        let jar = root.join("lib.jar");
        std::fs::write(&jar, "zip").expect("jar");
        std::fs::write(root.join("pom.xml"), "<project/>").expect("pom");
        let printed = format!(
            "echo '[INFO] Dependencies classpath:'\necho '{}'",
            jar.display()
        );
        let cache = root.join("cache/classpath.txt");
        let resolve = || {
            resolve_classpath_reporting(
                &root,
                BuildSystemKind::Maven,
                Some(&cache),
                &CancellationToken::new(),
                &ChildSlot::default(),
            )
            .expect("an answer")
        };
        fake_mvnw(&root, &format!("{printed}\nexit 1"));
        let partial = resolve();
        assert_eq!((partial.jars.len(), partial.complete), (1, false));
        fake_mvnw(&root, &printed);
        let whole = resolve();
        assert_eq!((whole.jars.len(), whole.complete), (1, true));
        fake_mvnw(&root, "exit 1");
        let cached = resolve();
        assert_eq!((cached.jars.len(), cached.complete), (1, true));
        assert!(has_build_files(&root));
        assert!(!has_build_files(&scratch("no-build-files")));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Gradle skips an unresolvable dependency leniently and exits 0; the
    /// init script says so, and that answer is neither whole nor kept —
    /// kept, it would outlive the network coming back.
    #[cfg(unix)]
    #[test]
    fn a_gradle_answer_with_a_skipped_dependency_is_not_whole_and_not_cached() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = scratch("gradle-lenient");
        let jar = root.join("lib.jar");
        std::fs::write(&jar, "zip").expect("jar");
        std::fs::write(root.join("build.gradle"), "").expect("build");
        let gradlew = root.join("gradlew");
        std::fs::write(
            &gradlew,
            format!(
                "#!/bin/sh\necho 'JAVELIN_CP={}'\necho 'JAVELIN_INCOMPLETE=:compileClasspath'\n",
                jar.display()
            ),
        )
        .expect("gradlew");
        std::fs::set_permissions(&gradlew, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let cache = root.join("cache/classpath.txt");
        let resolved = resolve_classpath_reporting(
            &root,
            BuildSystemKind::Gradle,
            Some(&cache),
            &CancellationToken::new(),
            &ChildSlot::default(),
        )
        .expect("an answer");
        assert_eq!((resolved.jars.len(), resolved.complete), (1, false));
        assert!(!cache.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `org.gradle.console=rich` in a project's `gradle.properties` makes
    /// Gradle draw its progress into piped stdout, wrapping every printed
    /// line in escape sequences: each path then names no file, and the
    /// classpath reads as broken. The wrapper here draws exactly so unless
    /// it is told `--console=plain`.
    #[cfg(unix)]
    #[test]
    fn a_rich_console_in_gradle_properties_does_not_garble_the_classpath() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = scratch("gradle-rich-console");
        let jar = root.join("lib.jar");
        std::fs::write(&jar, "zip").expect("jar");
        std::fs::write(root.join("build.gradle"), "").expect("build");
        let gradlew = root.join("gradlew");
        std::fs::write(
            &gradlew,
            format!(
                "#!/bin/sh\ncase \" $* \" in *' --console=plain '*) echo 'JAVELIN_CP={0}' ;;\n\
                 *) printf '\\033[1m> :app:javelinClasspath\\033[m\\033[5AJAVELIN_CP={0}\\033[0K\\n' \
                 ;; esac\n",
                jar.display()
            ),
        )
        .expect("gradlew");
        std::fs::set_permissions(&gradlew, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let resolved = resolve_classpath_reporting(
            &root,
            BuildSystemKind::Gradle,
            None,
            &CancellationToken::new(),
            &ChildSlot::default(),
        )
        .expect("an answer");
        assert_eq!((resolved.jars, resolved.complete), (vec![jar], true));
        let _ = std::fs::remove_dir_all(&root);
    }
}
