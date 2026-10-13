//! The dependency JARs' type facts: which JARs, from the build tool, and what
//! each declares, from its class files — each cached so a session whose build
//! files and JARs have not moved runs no process and reads no JAR.
//!
//! Runs on the classpath thread after the JDK's world is published, so a
//! build tool that takes minutes delays nothing the JDK already answers.

use std::path::{Path, PathBuf};
use std::time::Instant;

use javelin_build::BuildSystemKind;
use javelin_classpath::TypeFacts;
use javelin_core::config::BuildSystem;
use javelin_core::{CancellationToken, Config, Error};
use javelin_index::CacheDb;

/// What the dependencies contributed, and what diagnostics need to know
/// about how much of it there is.
#[derive(Debug, Default)]
pub(crate) struct Dependencies {
    /// Every dependency JAR's facts, in classpath order.
    pub facts: Vec<TypeFacts>,
    /// Nothing a file could name is missing from `facts`: the workspace has no
    /// build file anywhere, or the build tool answered in full and every JAR
    /// it named was read. `false` for a phase switched off with a build file
    /// present, which would otherwise flag every dependency type unknown.
    pub complete: bool,
    /// Some JAR registers an annotation processor, or a build file configures
    /// one, so the build generates types no source declares.
    pub processors: bool,
    /// No root holds a Maven or Gradle build file, so nothing was asked.
    pub loose: bool,
    /// A build file names a source generator — see [`javelin_build::BuildHints`].
    pub codegen: bool,
    /// A build file names a tool that adds members to declared types — see
    /// [`javelin_build::BuildHints::rewrites`].
    pub rewrites: bool,
    /// The lowest and highest Java level the build files name.
    pub release: Option<(u32, u32)>,
    /// What each module compiles against, when the build tool said.
    pub modules: Option<std::sync::Arc<crate::modules::Modules>>,
    /// The types only JARs the workspace's own build wrote hold — a
    /// module's `build/libs` JAR on its dependent's classpath — which
    /// [`javelin_classpath::TypeWorld::only_own_output`] answers from.
    pub own_output: std::collections::HashSet<String>,
    /// Which JARs these are — each one's path, size and modification time —
    /// so member references resolved through another classpath are owed.
    /// `0` when the build tool was not asked or did not answer.
    pub key: u64,
    /// JARs whose `-sources.jar` is on disk and not yet read into their
    /// store: grafted after `facts` is published, so a first read of the
    /// sources — JDK-scale on a machine an IDE fetched them onto — delays
    /// no JAR type.
    pub owed: Vec<Owed>,
    /// JARs with no `-sources.jar` on disk, for [`fetch_sources`] to ask the
    /// build tool for. `None` when nothing is to be asked.
    pub unsourced: Option<Unsourced>,
}

/// What [`fetch_sources`] asks for, and where.
#[derive(Debug)]
pub(crate) struct Unsourced {
    root: PathBuf,
    kind: BuildSystemKind,
    /// The workspace's cache directory: the runs' scratch files, and the
    /// record that this classpath was asked for.
    dir: PathBuf,
    /// [`Dependencies::key`]: a classpath already asked for is not asked
    /// again, so a library that publishes no sources costs one run.
    key: u64,
    /// Each JAR as an [`Owed`] with no sources yet, its version the bare
    /// store version.
    jars: Vec<Owed>,
}

/// A JAR's sources still to be grafted, and where its facts sit in
/// [`Dependencies::facts`].
#[derive(Debug)]
pub(crate) struct Owed {
    jar: PathBuf,
    sources: PathBuf,
    /// The store version the grafted facts are written under.
    version: String,
    processor: bool,
    whole: bool,
    range: std::ops::Range<usize>,
}

/// Graft every owed JAR's sources onto its facts in `facts`, storing each
/// one that read, and answer how many JARs took any. A sources JAR that will
/// not read, or a graft cancelled at shutdown, stores nothing: the next
/// session owes it again rather than never.
pub(crate) fn graft(
    config: &Config,
    owed: &[Owed],
    facts: &mut [TypeFacts],
    cancel: &CancellationToken,
) -> usize {
    let cache_root = javelin_core::paths::cache_root(config.index.cache_dir.as_deref()).ok();
    let mut grafted = 0;
    for owed in owed {
        let Some(slice) = facts.get_mut(owed.range.clone()) else {
            continue;
        };
        match javelin_classpath::graft_sources(
            slice,
            &owed.sources,
            config.index.max_file_size_bytes,
            cancel,
        ) {
            Ok(_) if !cancel.is_cancelled() => {}
            Ok(_) => return grafted,
            Err(err) => {
                tracing::debug!(sources = %owed.sources.display(), %err, "dependencies: sources JAR unread");
                continue;
            }
        }
        grafted += 1;
        if let Some(root) = &cache_root
            && config.index.persist
        {
            store(
                root,
                &owed.jar,
                &owed.version,
                slice,
                owed.processor,
                owed.whole,
            );
        }
    }
    grafted
}

/// Ask the build tool for `unsourced`'s sources JARs, and answer as owed each
/// JAR that has one on disk afterwards. A run that fails is asked again next
/// session; one that succeeds is not, until the classpath changes.
pub(crate) fn fetch_sources(
    unsourced: Unsourced,
    cancel: &CancellationToken,
    child: &javelin_build::ChildSlot,
) -> Vec<Owed> {
    let started = Instant::now();
    let jars: Vec<PathBuf> = unsourced.jars.iter().map(|owed| owed.jar.clone()).collect();
    match javelin_build::download_sources(
        &unsourced.root,
        unsourced.kind,
        &jars,
        &unsourced.dir.join("sources"),
        cancel,
        child,
    ) {
        Ok(true) => {
            let _ = std::fs::write(unsourced.dir.join(FETCHED), unsourced.key.to_string());
        }
        Ok(false) => {}
        Err(Error::Cancelled) => return Vec::new(),
        Err(err) => tracing::warn!(%err, "dependencies: no sources fetched"),
    }
    let found: Vec<Owed> = unsourced
        .jars
        .into_iter()
        .filter_map(|mut owed| {
            owed.sources = javelin_classpath::sources_jar_for(&owed.jar)?;
            owed.version = format!("{}+{}", owed.version, stamp(&owed.sources));
            Some(owed)
        })
        .collect();
    tracing::info!(
        asked = jars.len(),
        found = found.len(),
        duration_ms = started.elapsed().as_millis() as u64,
        "dependencies: sources fetched"
    );
    found
}

/// The file in the workspace's cache directory naming the classpath whose
/// sources were last fetched, by [`Dependencies::key`].
const FETCHED: &str = "sources-fetched.txt";

/// Write one JAR's facts to its store under `version`.
fn store(
    cache_root: &Path,
    jar: &Path,
    version: &str,
    facts: &[TypeFacts],
    processor: bool,
    whole: bool,
) {
    let home = jar.to_string_lossy().into_owned();
    let Ok(mut db) = CacheDb::open(&cache_root.join("jars").join(store_name(&home))) else {
        return;
    };
    let stored = db
        .write_types(&home, version, facts)
        .and_then(|()| db.set_metadata(PROCESSOR, if processor { "1" } else { "0" }))
        .and_then(|()| db.set_metadata(WHOLE, if whole { "1" } else { "0" }));
    if let Err(err) = stored {
        tracing::debug!(jar = %jar.display(), %err, "dependencies: JAR facts not cached");
    }
}

/// [`read_classpath`], and what the build files say besides.
pub(crate) fn read(
    config: &Config,
    roots: &[PathBuf],
    cancel: &CancellationToken,
    child: &javelin_build::ChildSlot,
) -> Dependencies {
    let mut dependencies = read_classpath(config, roots, cancel, child);
    dependencies.loose = !roots
        .iter()
        .any(|root| javelin_build::has_build_files(root));
    if let Some(root) = roots.first()
        && !dependencies.loose
    {
        let hints = javelin_build::build_hints(root);
        dependencies.processors |= hints.processors;
        dependencies.codegen = hints.codegen;
        dependencies.rewrites = hints.rewrites;
        dependencies.release = hints
            .releases
            .first()
            .zip(hints.releases.last())
            .map(|(low, high)| (*low, *high));
    }
    dependencies
}

/// The dependencies' facts, and how whole they are.
///
/// ponytail: every JAR's facts are read into memory at every start, cached or
/// not, like the JDK's — a monorepo's thousand JARs are a few hundred
/// megabytes. The upgrade is reading a JAR's facts when a name first needs it.
fn read_classpath(
    config: &Config,
    roots: &[PathBuf],
    cancel: &CancellationToken,
    child: &javelin_build::ChildSlot,
) -> Dependencies {
    let Some(root) = roots.first() else {
        return Dependencies {
            complete: true,
            ..Dependencies::default()
        };
    };
    // Only the first root's build is asked, so another root's build files
    // mean dependencies nothing asked for.
    let others_build = roots[1..]
        .iter()
        .any(|other| javelin_build::has_build_files(other));
    if !javelin_build::has_build_files(root) {
        return Dependencies {
            complete: !others_build,
            ..Dependencies::default()
        };
    }
    let incomplete = Dependencies::default();
    if !config.build.import_on_startup {
        return incomplete;
    }
    let kind = match config.build.system {
        BuildSystem::None => return incomplete,
        BuildSystem::Maven => BuildSystemKind::Maven,
        BuildSystem::Gradle => BuildSystemKind::Gradle,
        BuildSystem::Auto => match javelin_build::detect(root) {
            Some(kind) => kind,
            None => return incomplete,
        },
    };
    let Ok(cache_root) = javelin_core::paths::cache_root(config.index.cache_dir.as_deref()) else {
        return incomplete;
    };
    let started = Instant::now();
    tracing::info!(build = kind.name(), "dependencies: asking the build tool");
    let classpath =
        javelin_core::paths::workspace_cache_dir(&cache_root, root).join("classpath.txt");
    let resolved = match javelin_build::resolve_classpath_reporting(
        root,
        kind,
        // `persist` off: nothing kept, and nothing a previous session kept
        // believed — the rule the JDK's store follows.
        config.index.persist.then_some(classpath.as_path()),
        cancel,
        child,
    ) {
        Ok(resolved) => resolved,
        Err(Error::Cancelled) => return incomplete,
        Err(err) => {
            tracing::warn!(%err, "dependencies: no classpath; answering from the JDK and the workspace");
            return incomplete;
        }
    };
    let mut found = Dependencies {
        complete: resolved.complete && !others_build,
        key: jars_key(&resolved.jars),
        ..Dependencies::default()
    };
    // Where each classpath JAR's facts sit in `found.facts`, by its index.
    let mut ranges = Vec::with_capacity(resolved.jars.len());
    // A build tool prints a path with its symlinks resolved, so the roots
    // are compared resolved too.
    let resolved_roots: Vec<PathBuf> = roots
        .iter()
        .map(|root| std::fs::canonicalize(root).unwrap_or_else(|_| root.clone()))
        .collect();
    let mut elsewhere = std::collections::HashSet::new();
    let dir = javelin_core::paths::workspace_cache_dir(&cache_root, root);
    let fetch = config.build.download_sources
        && !(config.index.persist
            && std::fs::read_to_string(dir.join(FETCHED))
                .is_ok_and(|fetched| fetched == found.key.to_string()));
    let mut unsourced = Vec::new();
    for jar in &resolved.jars {
        if cancel.is_cancelled() {
            return incomplete;
        }
        match jar_facts(config, &cache_root, jar, cancel) {
            Some(read) => {
                let own = javelin_index::is_build_output(&resolved_roots, jar)
                    || javelin_index::is_build_output(roots, jar);
                let fqns = read.facts.iter().map(|fact| fact.fqn.clone());
                if own {
                    found.own_output.extend(fqns);
                } else {
                    elsewhere.extend(fqns);
                }
                let start = found.facts.len();
                found.facts.extend(read.facts);
                ranges.push(start..found.facts.len());
                found.processors |= read.processor;
                found.complete &= read.whole;
                if let Some((sources, version)) = read.owed {
                    found.owed.push(Owed {
                        jar: jar.clone(),
                        sources,
                        version,
                        processor: read.processor,
                        whole: read.whole,
                        range: start..found.facts.len(),
                    });
                } else if fetch
                    // A JAR the checkout holds, own output or vendored, is
                    // not fetched for: the fetch writes where the JAR is.
                    && !roots.iter().chain(&resolved_roots).any(|root| jar.starts_with(root))
                    && start < found.facts.len()
                    && javelin_classpath::sources_jar_for(jar).is_none()
                {
                    unsourced.push(Owed {
                        jar: jar.clone(),
                        sources: PathBuf::new(),
                        version: stamp(jar),
                        processor: read.processor,
                        whole: read.whole,
                        range: start..found.facts.len(),
                    });
                }
            }
            None => {
                found.complete = false;
                ranges.push(0..0);
            }
        }
    }
    // A type another JAR holds too is two copies, and which one a file
    // compiles against is not the workspace's to say.
    found.own_output.retain(|fqn| !elsewhere.contains(fqn));
    found.unsourced = (!unsourced.is_empty()).then(|| Unsourced {
        root: root.clone(),
        kind,
        dir,
        key: found.key,
        jars: unsourced,
    });
    if !resolved.modules.is_empty() {
        found.modules = Some(std::sync::Arc::new(crate::modules::Modules::new(
            ranges
                .iter()
                .enumerate()
                .map(|(jar, range)| (jar, &found.facts[range.clone()])),
            resolved.modules,
        )));
    }
    tracing::info!(
        jars = resolved.jars.len(),
        types = found.facts.len(),
        complete = found.complete,
        processors = found.processors,
        own_output = found.own_output.len(),
        modules = found.modules.as_ref().map_or(0, |modules| modules.len()),
        duration_ms = started.elapsed().as_millis() as u64,
        "dependencies ready"
    );
    found
}

/// [`Dependencies::key`] for `jars`.
fn jars_key(jars: &[PathBuf]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    // Gradle configures projects in parallel and prints one JAR set in a new
    // order each run; an order in the key would owe every file's member
    // references again every session.
    let mut jars: Vec<&PathBuf> = jars.iter().collect();
    jars.sort();
    for jar in jars {
        jar.hash(&mut hasher);
        if let Ok(meta) = std::fs::metadata(jar) {
            meta.len().hash(&mut hasher);
            meta.modified().ok().hash(&mut hasher);
        }
    }
    // Never the key of no answer.
    hasher.finish() | 1
}

/// One JAR's facts, as [`jar_facts`] reads them.
struct JarRead {
    facts: Vec<TypeFacts>,
    /// It registers an annotation processor.
    processor: bool,
    /// No class in it was dropped — too large, or unreadable.
    whole: bool,
    /// Its sources JAR and the store version grafted facts go under, when
    /// one is on disk and its store does not hold them yet.
    owed: Option<(PathBuf, String)>,
}

/// One JAR's facts and whether it registers an annotation processor, from its
/// store under `<cache root>/jars/` when the JAR's modification time and size
/// are the stored ones, and read otherwise. `None` for a JAR that cannot be
/// read.
///
/// ponytail: one store file per JAR, which reuses the JDK's store unchanged
/// and costs a file open per JAR per start. The upgrade is one store keyed by
/// JAR.
fn jar_facts(
    config: &Config,
    cache_root: &Path,
    jar: &Path,
    cancel: &CancellationToken,
) -> Option<JarRead> {
    let home = jar.to_string_lossy().into_owned();
    // The sources beside it are part of what was read, so a sources JAR
    // fetched after the first read is a version the store does not hold.
    let bare = stamp(jar);
    let sources = javelin_classpath::sources_jar_for(jar);
    let sited = sources
        .as_ref()
        .map(|sources| format!("{bare}+{}", stamp(sources)));
    let db = config
        .index
        .persist
        .then(|| CacheDb::open(&cache_root.join("jars").join(store_name(&home))).ok())
        .flatten();
    let cached = |version: &str| {
        let db = db.as_ref()?;
        let facts = crate::classpath::cached(db, &home, version)?;
        let processor = db.metadata(PROCESSOR).ok()??;
        let whole = db.metadata(WHOLE).ok()??;
        Some(JarRead {
            facts,
            processor: processor == "1",
            whole: whole == "1",
            owed: None,
        })
    };
    if let Some(read) = sited.as_deref().and_then(cached) {
        return Some(read);
    }
    let owed = sources.zip(sited);
    if let Some(read) = cached(&bare) {
        return Some(JarRead { owed, ..read });
    }
    let (facts, whole) = match javelin_classpath::read_jar_facts_reporting(
        jar,
        config.index.max_file_size_bytes,
        cancel,
    ) {
        Ok(read) => read,
        Err(err) => {
            tracing::debug!(jar = %jar.display(), %err, "dependencies: JAR unreadable, skipped");
            return None;
        }
    };
    let processor = javelin_classpath::registers_processor(jar);
    if db.is_some() && !facts.is_empty() {
        store(cache_root, jar, &bare, &facts, processor, whole);
    }
    Some(JarRead {
        facts,
        processor,
        whole,
        owed,
    })
}

/// A file's modification time and size: what says it is the one read before.
fn stamp(path: &Path) -> String {
    std::fs::metadata(path)
        .map(|metadata| {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |since| since.as_nanos());
            format!("{modified}-{}", metadata.len())
        })
        .unwrap_or_default()
}

/// The per-JAR store's record of [`javelin_classpath::registers_processor`].
const PROCESSOR: &str = "jar.processor";

/// The per-JAR store's record of whether every class was read. A store from
/// before it was recorded misses it and the JAR is read again.
const WHOLE: &str = "jar.whole";

fn store_name(path: &str) -> String {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    format!("{:016x}.sqlite", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_world_key_does_not_depend_on_the_order_the_build_tool_printed() {
        let (a, b) = (PathBuf::from("/m2/a.jar"), PathBuf::from("/m2/b.jar"));
        assert_eq!(
            jars_key(&[a.clone(), b.clone()]),
            jars_key(&[b.clone(), a.clone()])
        );
        assert_ne!(jars_key(std::slice::from_ref(&a)), jars_key(&[a, b]));
    }

    fn acme_jar() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/classfiles/acme.jar")
            .canonicalize()
            .expect("the fixture JAR is checked in")
    }

    /// `acme.jar` as `<dir>/acme-1.0.jar`, with its sources beside it when
    /// `sources` — the checked-in `src/` the class files were compiled from.
    pub(crate) fn acme_with_sources(dir: &Path, sources: bool) -> PathBuf {
        use std::io::Write as _;
        std::fs::create_dir_all(dir).expect("dir");
        let jar = dir.join("acme-1.0.jar");
        std::fs::copy(acme_jar(), &jar).expect("copy");
        if sources {
            let src = acme_jar().with_file_name("src");
            let mut writer = zip::ZipWriter::new(
                std::fs::File::create(dir.join("acme-1.0-sources.jar")).expect("sources"),
            );
            let options = zip::write::SimpleFileOptions::default();
            for entry in std::fs::read_dir(src.join("com/acme"))
                .expect("src")
                .flatten()
            {
                let name = format!("com/acme/{}", entry.file_name().to_string_lossy());
                writer.start_file(name, options).expect("entry");
                writer
                    .write_all(&std::fs::read(entry.path()).expect("read"))
                    .expect("write");
            }
            writer.finish().expect("finish");
        }
        jar
    }

    /// Both passes, as the classpath thread runs them: the JAR's facts, then
    /// its sources grafted onto them.
    fn read_sited(config: &Config, cache: &Path, jar: &Path) -> Vec<TypeFacts> {
        let read = jar_facts(config, cache, jar, &CancellationToken::new()).expect("read");
        let mut facts = read.facts;
        if let Some((sources, version)) = read.owed {
            let owed = Owed {
                jar: jar.to_path_buf(),
                sources,
                version,
                processor: read.processor,
                whole: read.whole,
                range: 0..facts.len(),
            };
            graft(config, &[owed], &mut facts, &CancellationToken::new());
        }
        facts
    }

    /// A sources JAR beside the JAR is owed until grafted and stored; one
    /// fetched after the first read is a version the store does not hold;
    /// and one that will not read stores nothing, so it stays owed.
    #[test]
    fn a_sources_jar_beside_a_jar_sites_its_facts_and_keys_the_store() {
        let dir = scratch("sources-jar");
        let cache = dir.join("cache");
        let config = config_with_cache(&cache);
        let jar = acme_with_sources(&dir.join("m2"), false);
        let first = || jar_facts(&config, &cache, &jar, &CancellationToken::new()).expect("read");
        let box_of = |facts: &[TypeFacts]| {
            facts
                .iter()
                .find(|facts| facts.fqn == "com.acme.Box")
                .expect("Box")
                .clone()
        };
        let bare = first();
        assert!(bare.owed.is_none() && box_of(&bare.facts).source.is_empty());

        std::fs::write(dir.join("m2/acme-1.0-sources.jar"), "not a zip").expect("corrupt");
        assert!(first().owed.is_some(), "owed, not read on the first pass");
        read_sited(&config, &cache, &jar);
        assert!(
            first().owed.is_some(),
            "a sources JAR that would not read stays owed"
        );

        acme_with_sources(&dir.join("m2"), true);
        let owing = first();
        assert!(owing.owed.is_some() && box_of(&owing.facts).source.is_empty());
        let sited = box_of(&read_sited(&config, &cache, &jar));
        assert!(
            sited
                .source
                .ends_with("acme-1.0-sources.jar!/com/acme/Box.java"),
            "{}",
            sited.source
        );
        assert!(sited.name_end > sited.name_start);
        let get = sited
            .members
            .iter()
            .find(|member| member.name == "get")
            .expect("get");
        assert!(get.name_end > get.name_start, "one `get`, so sited");
        // And the store now gives the sited facts back, owing nothing.
        let stored = first();
        assert!(stored.owed.is_none());
        assert_eq!(box_of(&stored.facts).source, sited.source);
    }

    /// From a buffer, a JAR member and a JAR type jump into the entry the
    /// sources JAR holds, extracted under the cache root.
    #[test]
    fn definition_on_a_jar_member_and_type_lands_in_its_sources_jar() {
        let dir = scratch("sources-jump");
        let cache = dir.join("cache");
        let config = config_with_cache(&cache);
        let jar = acme_with_sources(&dir.join("m2"), true);
        let facts = read_sited(&config, &cache, &jar);
        let mut state = crate::ServerState::new(config).expect("grammar loads");
        state.install_classpath(crate::classpath::Classpath::holding(facts));
        state.install_workspace(crate::workspace::Workspace {
            index: std::sync::Arc::new(javelin_index::WorkspaceIndex::default()),
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: Some(std::sync::Arc::default()),
            absorbed: 0,
        });
        let uri = "file:///A.java";
        let text = "import com.acme.Box;\nclass A { void m(Box<String> b) { b.get(); } }\n";
        state.did_open(javelin_document::DocumentId::new(uri), 1, text.to_owned());
        let jump = |state: &mut crate::ServerState, needle: &str| {
            let offset = text.find(needle).expect("in the text") + 1;
            let line = text[..offset].matches('\n').count() as u32;
            let character = (offset - text[..offset].rfind('\n').map_or(0, |n| n + 1)) as u32;
            let params: lsp_types::GotoDefinitionParams =
                serde_json::from_value(serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character },
                }))
                .expect("params");
            match crate::handlers::definition::handle(state, &params, &CancellationToken::new())
                .expect("not cancelled")
            {
                Some(lsp_types::GotoDefinitionResponse::Scalar(location)) => location,
                other => panic!("one location for {needle}, got {other:?}"),
            }
        };
        let source =
            std::fs::read_to_string(acme_jar().with_file_name("src").join("com/acme/Box.java"))
                .expect("source");
        for (needle, expected) in [("get()", "get"), ("Box<String>", "Box")] {
            let location = jump(&mut state, needle);
            let path = crate::workspace::path_of(location.uri.as_str()).expect("a path");
            assert!(
                path.starts_with(cache.join("jar-sources")),
                "{}",
                path.display()
            );
            assert!(path.ends_with("com/acme/Box.java"));
            let line = source
                .lines()
                .nth(location.range.start.line as usize)
                .expect("line");
            let start = location.range.start.character as usize;
            let end = location.range.end.character as usize;
            assert_eq!(&line[start..end], expected);
        }

        // A member the sources could not tell from an overload keeps no span,
        // and has no site rather than the top of the file.
        let mut facts = read_sited(&state.config, &cache, &jar);
        for member in facts
            .iter_mut()
            .filter(|facts| facts.fqn == "com.acme.Box")
            .flat_map(|facts| facts.members.iter_mut())
            .filter(|member| member.name == "get")
        {
            member.name_start = 0;
            member.name_end = 0;
        }
        state.install_classpath(crate::classpath::Classpath::holding(facts));
        let offset = text.find("get()").expect("in the text") + 1;
        let params: lsp_types::GotoDefinitionParams = serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 1, "character": offset - text.find('\n').expect("a line") - 1 },
        }))
        .expect("params");
        assert_eq!(
            crate::handlers::definition::handle(&mut state, &params, &CancellationToken::new())
                .expect("not cancelled"),
            None
        );
    }

    /// A sources JAR fetched again after its offsets were read: the jump
    /// extracts the new text, which no longer writes the name where the old
    /// one did, and answers nothing rather than landing on what does.
    #[test]
    fn a_sources_jar_replaced_after_its_offsets_were_read_jumps_nowhere() {
        use std::io::Write as _;
        let dir = scratch("sources-replaced");
        let cache = dir.join("cache");
        let config = config_with_cache(&cache);
        let jar = acme_with_sources(&dir.join("m2"), true);
        let facts = read_sited(&config, &cache, &jar);
        let box_java = acme_jar().with_file_name("src").join("com/acme/Box.java");
        let shifted = format!(
            "// a new release\n{}",
            std::fs::read_to_string(&box_java).expect("src")
        );
        let mut writer = zip::ZipWriter::new(
            std::fs::File::create(dir.join("m2/acme-1.0-sources.jar")).expect("sources"),
        );
        writer
            .start_file(
                "com/acme/Box.java",
                zip::write::SimpleFileOptions::default(),
            )
            .expect("entry");
        writer.write_all(shifted.as_bytes()).expect("write");
        writer.finish().expect("finish");

        let mut state = crate::ServerState::new(config).expect("grammar loads");
        state.install_classpath(crate::classpath::Classpath::holding(facts));
        let uri = "file:///A.java";
        let text = "import com.acme.Box;\nclass A { void m(Box<String> b) { b.get(); } }\n";
        state.did_open(javelin_document::DocumentId::new(uri), 1, text.to_owned());
        let character = text.lines().nth(1).expect("line").find("get").expect("get") + 1;
        let params: lsp_types::GotoDefinitionParams = serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 1, "character": character },
        }))
        .expect("params");
        assert_eq!(
            crate::handlers::definition::handle(&mut state, &params, &CancellationToken::new())
                .expect("not cancelled"),
            None
        );
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("javelin-deps-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[cfg(unix)]
    fn maven_project(name: &str) -> PathBuf {
        let root = scratch(name);
        write_mvnw(&root, &[acme_jar()]);
        root
    }

    /// A `pom.xml` and an `mvnw` that answers `jars` as the classpath.
    #[cfg(unix)]
    fn write_mvnw(root: &Path, jars: &[PathBuf]) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(root.join("pom.xml"), "<project/>").expect("pom");
        let mvnw = root.join("mvnw");
        let classpath: Vec<String> = jars.iter().map(|jar| jar.display().to_string()).collect();
        std::fs::write(
            &mvnw,
            format!(
                "#!/bin/sh\necho '[INFO] Dependencies classpath:'\necho '{}'\n",
                classpath.join(":")
            ),
        )
        .expect("mvnw");
        std::fs::set_permissions(&mvnw, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    /// A JAR below one of the workspace's build directories is its own
    /// output, and a type another JAR holds too is not only its.
    #[cfg(unix)]
    #[test]
    fn a_jar_the_workspaces_build_wrote_is_its_own_output() {
        let root = scratch("own-output");
        let own = root.join("core/build/libs/core.jar");
        std::fs::create_dir_all(own.parent().expect("parent")).expect("dirs");
        std::fs::copy(acme_jar(), &own).expect("copy");
        let cache = root.with_extension("cache");
        let config = config_with_cache(&cache);
        let read_with = |jars: &[PathBuf]| {
            write_mvnw(&root, jars);
            read(
                &config,
                std::slice::from_ref(&root),
                &CancellationToken::new(),
                &Default::default(),
            )
        };
        let alone = read_with(std::slice::from_ref(&own));
        assert!(alone.complete);
        assert!(
            alone.own_output.contains("com.acme.Box"),
            "{:?}",
            alone.own_output
        );
        let twice = read_with(&[own, acme_jar()]);
        assert!(
            !twice.own_output.contains("com.acme.Box"),
            "{:?}",
            twice.own_output
        );
        let elsewhere = read_with(&[acme_jar()]);
        assert!(elsewhere.own_output.is_empty());
        // Sources are fetched to where a JAR is, so never for one the
        // checkout holds: its own output, or one it vendors.
        assert!(elsewhere.unsourced.is_some());
        assert!(alone.unsourced.is_none());
        let vendored = root.join("repository/org/x/a/1.0/a-1.0.jar");
        std::fs::create_dir_all(vendored.parent().expect("parent")).expect("dirs");
        std::fs::copy(acme_jar(), &vendored).expect("copy");
        assert!(read_with(&[vendored]).unsourced.is_none());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// A JAR with no sources on disk is asked of the build tool, and what it
    /// fetched is owed and grafted. A classpath asked for once is not asked
    /// again, whether or not the fetch found anything; switched off, nothing
    /// is asked.
    #[cfg(unix)]
    #[test]
    fn a_jar_without_sources_has_them_fetched_once_per_classpath() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = scratch("fetch");
        let cache = root.with_extension("cache");
        let m2 = root.with_extension("m2");
        let jar = acme_with_sources(&m2, false);
        let stash = root.with_extension("stash");
        acme_with_sources(&stash, true);
        let asked = root.with_extension("asked");
        std::fs::write(root.join("pom.xml"), "<project/>").expect("pom");
        let mvnw = root.join("mvnw");
        std::fs::write(
            &mvnw,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n*dependency:sources*) echo asked >> '{}'; \
                 cp '{}' '{}';;\n*) echo '[INFO] Dependencies classpath:'; echo '{}';;\nesac\n",
                asked.display(),
                stash.join("acme-1.0-sources.jar").display(),
                m2.display(),
                jar.display()
            ),
        )
        .expect("mvnw");
        std::fs::set_permissions(&mvnw, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let read_with = |config: &Config| {
            read(
                config,
                std::slice::from_ref(&root),
                &CancellationToken::new(),
                &Default::default(),
            )
        };
        let mut off = config_with_cache(&cache);
        off.build.download_sources = false;
        assert!(read_with(&off).unsourced.is_none());

        let config = config_with_cache(&cache);
        let first = read_with(&config);
        assert!(first.owed.is_empty());
        let owed = fetch_sources(
            first.unsourced.expect("asked for"),
            &CancellationToken::new(),
            &Default::default(),
        );
        assert_eq!(owed.len(), 1);
        let mut facts = first.facts;
        assert_eq!(
            graft(&config, &owed, &mut facts, &CancellationToken::new()),
            1
        );
        let source = &facts
            .iter()
            .find(|facts| facts.fqn == "com.acme.Box")
            .expect("Box")
            .source;
        assert!(
            source.ends_with("acme-1.0-sources.jar!/com/acme/Box.java"),
            "{source}"
        );

        let second = read_with(&config);
        assert!(second.unsourced.is_none() && second.owed.is_empty());
        // A JAR whose store holds its grafted facts has its sources, whatever
        // was asked before.
        let cache_root = javelin_core::paths::cache_root(Some(&cache)).expect("cache root");
        std::fs::remove_file(
            javelin_core::paths::workspace_cache_dir(&cache_root, &root).join(FETCHED),
        )
        .expect("the fetch was recorded");
        let third = read_with(&config);
        assert!(third.unsourced.is_none() && third.owed.is_empty());
        std::fs::write(
            javelin_core::paths::workspace_cache_dir(&cache_root, &root).join(FETCHED),
            third.key.to_string(),
        )
        .expect("record");
        // Gone again: the store's bare facts, and still not asked.
        std::fs::remove_file(m2.join("acme-1.0-sources.jar")).expect("remove");
        assert!(read_with(&config).unsourced.is_none());
        assert_eq!(std::fs::read_to_string(&asked).expect("asked"), "asked\n");
        for dir in [root, cache, m2, stash] {
            let _ = std::fs::remove_dir_all(dir);
        }
        let _ = std::fs::remove_file(asked);
    }

    fn config_with_cache(cache: &Path) -> Config {
        let mut config = Config::default();
        config.index.cache_dir = Some(cache.to_path_buf());
        config.build.import_on_startup = true;
        config
    }

    #[cfg(unix)]
    #[test]
    fn a_maven_project_yields_its_jars_facts_and_the_cache_gives_the_same_back() {
        let root = maven_project("maven");
        let cache = root.with_extension("cache");
        let config = config_with_cache(&cache);
        let fresh = read(
            &config,
            std::slice::from_ref(&root),
            &CancellationToken::new(),
            &Default::default(),
        );
        assert!(fresh.complete && !fresh.processors);
        let fresh = fresh.facts;
        assert!(
            fresh.iter().any(|facts| facts.fqn == "com.acme.Box"),
            "{:?}",
            fresh.iter().map(|facts| &facts.fqn).collect::<Vec<_>>()
        );
        let cached = read(
            &config,
            std::slice::from_ref(&root),
            &CancellationToken::new(),
            &Default::default(),
        );
        assert!(
            cached.complete && !cached.processors,
            "the store remembers both"
        );
        let cached = cached.facts;
        let mut fresh_fqns: Vec<&str> = fresh.iter().map(|facts| facts.fqn.as_str()).collect();
        let mut cached_fqns: Vec<&str> = cached.iter().map(|facts| facts.fqn.as_str()).collect();
        fresh_fqns.sort_unstable();
        cached_fqns.sort_unstable();
        assert_eq!(fresh_fqns, cached_fqns);
        let box_members = |facts: &[TypeFacts]| {
            facts
                .iter()
                .find(|facts| facts.fqn == "com.acme.Box")
                .map(|facts| facts.members.len())
        };
        assert_eq!(box_members(&fresh), box_members(&cached));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// A class the reader could not read is a type missing from the facts,
    /// so the dependencies are not whole — fresh, and out of the store.
    #[cfg(unix)]
    #[test]
    fn a_jar_with_a_class_that_cannot_be_read_is_not_whole() {
        use std::io::Write as _;
        let root = maven_project("unreadable-class");
        let jar = root.join("partial.jar");
        {
            let mut from = zip::ZipArchive::new(std::fs::File::open(acme_jar()).expect("fixture"))
                .expect("zip");
            let mut to = zip::ZipWriter::new(std::fs::File::create(&jar).expect("jar"));
            for index in 0..from.len() {
                to.raw_copy_file(from.by_index(index).expect("entry"))
                    .expect("copy");
            }
            to.start_file(
                "com/acme/Broken.class",
                zip::write::SimpleFileOptions::default(),
            )
            .expect("entry");
            to.write_all(b"not a class file").expect("write");
            to.finish().expect("finish");
        }
        std::fs::write(
            root.join("mvnw"),
            format!(
                "#!/bin/sh\necho '[INFO] Dependencies classpath:'\necho '{}'\n",
                jar.display()
            ),
        )
        .expect("mvnw");
        let cache = root.with_extension("cache");
        let config = config_with_cache(&cache);
        for read_through in ["fresh", "cached"] {
            let read = read(
                &config,
                std::slice::from_ref(&root),
                &CancellationToken::new(),
                &Default::default(),
            );
            assert!(!read.facts.is_empty(), "{read_through}");
            assert!(!read.complete, "{read_through}");
        }
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// A processor on Maven's processor path is on no classpath the tool
    /// prints, so the build file is what says one is there.
    #[cfg(unix)]
    #[test]
    fn a_processor_path_and_a_java_level_are_read_off_the_build_file() {
        let root = maven_project("processor-path");
        std::fs::write(
            root.join("pom.xml"),
            "<project><properties><maven.compiler.release>17</maven.compiler.release>\
             </properties><annotationProcessorPaths/></project>",
        )
        .expect("pom");
        let cache = root.with_extension("cache");
        let read = read(
            &config_with_cache(&cache),
            std::slice::from_ref(&root),
            &CancellationToken::new(),
            &Default::default(),
        );
        assert!(read.processors && !read.loose && !read.codegen);
        assert_eq!(read.release, Some((17, 17)));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// A JAR member completes and hovers, and has nowhere to jump: a class
    /// file has no text, so the member carries no site rather than one that
    /// names an empty entry.
    #[test]
    fn a_jar_member_is_answered_with_no_site() {
        use javelin_semantic::Externals as _;
        let facts =
            javelin_classpath::read_jar_facts(&acme_jar(), 1 << 20, &CancellationToken::new())
                .expect("the fixture JAR");
        let externals = crate::classpath::ClasspathExternals::new(
            std::sync::Arc::new(javelin_classpath::TypeWorld::new(facts)),
            std::sync::Arc::new(javelin_index::WorkspaceIndex::default()),
        );
        let members = externals.members("com.acme.Box").expect("held");
        let get = members
            .members
            .iter()
            .find(|member| member.name == "get")
            .expect("Box.get");
        assert_eq!(get.site, None);
        // `Box<T>`'s own variable, which a receiver's argument binds — never
        // the `Comparable` its bound erases to.
        assert_eq!(members.type_parameters, vec!["T"]);
        assert_eq!(
            get.declared,
            javelin_semantic::ExternalType::Variable("T".to_owned())
        );
    }

    #[cfg(unix)]
    #[test]
    fn switched_off_or_with_no_build_file_nothing_is_run() {
        let root = maven_project("off");
        let cache = root.with_extension("cache");
        let read_at = |config: &Config, root: &Path| {
            read(
                config,
                std::slice::from_ref(&root.to_path_buf()),
                &CancellationToken::new(),
                &Default::default(),
            )
        };
        // A build file and nothing asked: no facts, and not whole — every
        // dependency type would otherwise be flagged unknown.
        let mut config = config_with_cache(&cache);
        config.build.import_on_startup = false;
        let off = read_at(&config, &root);
        assert!(off.facts.is_empty() && !off.complete);
        config.build.import_on_startup = true;
        config.build.system = BuildSystem::None;
        let none = read_at(&config, &root);
        assert!(none.facts.is_empty() && !none.complete);
        // No build file anywhere: nothing to ask, and nothing missing.
        let loose = scratch("loose");
        let loose_read = read_at(&config_with_cache(&cache), &loose);
        assert!(loose_read.facts.is_empty() && loose_read.complete);
        // A module one level down is a build all the same.
        let nested = scratch("nested");
        std::fs::create_dir_all(nested.join("module")).expect("module");
        std::fs::write(nested.join("module/pom.xml"), "<project/>").expect("pom");
        let mut off = config_with_cache(&cache);
        off.build.import_on_startup = false;
        assert!(!read_at(&off, &nested).complete);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&loose);
        let _ = std::fs::remove_dir_all(&nested);
        let _ = std::fs::remove_dir_all(&cache);
    }
}
