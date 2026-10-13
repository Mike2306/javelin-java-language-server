//! Asking Maven or Gradle to download the dependencies' `-sources.jar`s.
//!
//! Each one lands where the tool keeps it, which is where
//! `javelin_classpath::sources_jar_for` already looks: beside the JAR in a
//! Maven repository, or in a sibling hash directory of Gradle's cache. So
//! nothing is paired here; the caller asks that function again afterwards.
//!
//! Two runs at most. The project's own tool first, through its own
//! repositories. Then `mvn` over a generated POM, for a JAR in a Maven
//! repository that still has no sources beside it. That is a Gradle build's
//! `mavenLocal()` component, for which Gradle asks the local repository
//! alone and never fetches sources.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use javelin_core::{CancellationToken, Error, Result};

use crate::BuildSystemKind;
use crate::classpath::{ChildSlot, GRADLE_ARGS, run, tool, wrapper};

/// Resolves every compile classpath's module components' sources artifacts.
/// Reading an artifact's file is what downloads it, so the script prints
/// nothing.
const GRADLE_SOURCES: &str = r#"allprojects {
    tasks.register('javelinSources') {
        doLast {
            def ids = [] as Set
            project.configurations.findAll { configuration ->
                configuration.canBeResolved &&
                    (configuration.name == 'compileClasspath' ||
                        configuration.name.endsWith('CompileClasspath'))
            }.each { configuration ->
                configuration.incoming.artifactView { lenient = true }.artifacts.each {
                    def id = it.id.componentIdentifier
                    if (id instanceof org.gradle.api.artifacts.component.ModuleComponentIdentifier) {
                        ids << id
                    }
                }
            }
            if (ids.isEmpty()) {
                return
            }
            project.dependencies.createArtifactResolutionQuery()
                .forComponents(ids)
                .withArtifacts(org.gradle.jvm.JvmLibrary, org.gradle.language.base.artifact.SourcesArtifact)
                .execute()
                .resolvedComponents.each { component ->
                    component.getArtifacts(org.gradle.language.base.artifact.SourcesArtifact).each {
                        if (it instanceof org.gradle.api.artifacts.result.ResolvedArtifactResult) {
                            it.file
                        }
                    }
                }
        }
    }
}
"#;

/// Download what sources the build's repositories hold for `jars`, using
/// `scratch` for the files the runs need. Answers whether the project's own
/// tool succeeded; the `mvn` run after it is best effort, since a build need
/// not have `mvn` and the generated POM names none of the build's
/// repositories. A sources JAR no repository holds is not a failure.
///
/// Runs the project's build logic, as [`crate::resolve_classpath`] does.
pub fn download_sources(
    root: &Path,
    kind: BuildSystemKind,
    jars: &[PathBuf],
    scratch: &Path,
    cancel: &CancellationToken,
    child: &ChildSlot,
) -> Result<bool> {
    download_with(
        OsStr::new(tool("mvn", "mvn.cmd")),
        root,
        kind,
        jars,
        scratch,
        cancel,
        child,
    )
}

/// [`download_sources`], with `maven` for the run over the generated POM.
fn download_with(
    maven: &OsStr,
    root: &Path,
    kind: BuildSystemKind,
    jars: &[PathBuf],
    scratch: &Path,
    cancel: &CancellationToken,
    child: &ChildSlot,
) -> Result<bool> {
    std::fs::create_dir_all(scratch).map_err(|source| Error::io(scratch, source))?;
    let (succeeded, _) = match kind {
        BuildSystemKind::Maven => {
            let mut command = Command::new(
                wrapper(root, "mvnw", "mvnw.cmd").unwrap_or(tool("mvn", "mvn.cmd").into()),
            );
            command.args([
                "-B",
                "-q",
                "--fail-at-end",
                "-Dstyle.color=never",
                "dependency:sources",
                "-DfailOnMissingClassifierArtifact=false",
            ]);
            run(command, root, cancel, child, kind)?
        }
        BuildSystemKind::Gradle => {
            let script = scratch.join("javelin-sources.gradle");
            std::fs::write(&script, GRADLE_SOURCES).map_err(|source| Error::io(&script, source))?;
            let mut command = Command::new(
                wrapper(root, "gradlew", "gradlew.bat")
                    .unwrap_or(tool("gradle", "gradle.bat").into()),
            );
            command
                .args(GRADLE_ARGS)
                .arg("--init-script")
                .arg(&script)
                .arg("javelinSources");
            run(command, root, cancel, child, kind)?
        }
    };
    from_maven_repositories(maven, jars, scratch, cancel, child)?;
    Ok(succeeded)
}

/// One `maven` run per Maven repository that holds a JAR of `jars` with no
/// sources beside it, fetching them into that repository. Only a
/// cancellation is an error; a run that fails or cannot start fetches
/// nothing.
fn from_maven_repositories(
    maven: &OsStr,
    jars: &[PathBuf],
    scratch: &Path,
    cancel: &CancellationToken,
    child: &ChildSlot,
) -> Result<()> {
    let mut repositories: BTreeMap<PathBuf, Vec<[String; 3]>> = BTreeMap::new();
    for jar in jars {
        if let Some((repository, coordinate)) = coordinates(jar)
            && !jar
                .with_file_name(format!("{}-{}-sources.jar", coordinate[1], coordinate[2]))
                .exists()
        {
            repositories.entry(repository).or_default().push(coordinate);
        }
    }
    for (index, (repository, coordinates)) in repositories.iter().enumerate() {
        let dir = scratch.join(format!("sources-{index}"));
        std::fs::create_dir_all(&dir).map_err(|source| Error::io(&dir, source))?;
        let pom = dir.join("pom.xml");
        std::fs::write(&pom, sources_pom(coordinates)).map_err(|source| Error::io(&pom, source))?;
        let mut command = Command::new(maven);
        command
            .args([
                "-B",
                "-q",
                "-Dstyle.color=never",
                "-f",
                "pom.xml",
                "dependency:sources",
                "-DfailOnMissingClassifierArtifact=false",
                "-DexcludeTransitive=true",
            ])
            .arg({
                let mut local = std::ffi::OsString::from("-Dmaven.repo.local=");
                local.push(repository);
                local
            });
        match run(command, &dir, cancel, child, BuildSystemKind::Maven) {
            Ok(_) => {}
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            // No `mvn` on the path: a Gradle build need not have one.
            Err(err) => {
                tracing::debug!(%err, "build: no Maven run for the local repository's sources");
            }
        }
    }
    Ok(())
}

/// A JAR's Maven repository, group, artifact and version, read off its path
/// `<repository>/<group as directories>/<artifact>/<version>/<artifact>-<version>.jar`,
/// where the repository is the nearest directory above named `repository`.
/// `None` for any other path, a classified JAR, or a part that is not a plain
/// name, since each is written into a POM.
///
/// shortcut: a group with a segment named `repository` is cut at it, which
/// asks for an artifact that does not exist and fetches nothing. The upgrade
/// is the repository root from the build tool's settings.
fn coordinates(jar: &Path) -> Option<(PathBuf, [String; 3])> {
    let version_dir = jar.parent()?;
    let artifact_dir = version_dir.parent()?;
    let version = version_dir.file_name()?.to_str()?;
    let artifact = artifact_dir.file_name()?.to_str()?;
    if jar.file_name()?.to_str()? != format!("{artifact}-{version}.jar") {
        return None;
    }
    let mut group = Vec::new();
    let mut dir = artifact_dir.parent()?;
    loop {
        let name = dir.file_name()?.to_str()?;
        if name == "repository" {
            break;
        }
        group.push(name);
        dir = dir.parent()?;
    }
    if group.is_empty() {
        return None;
    }
    group.reverse();
    let coordinate = [group.join("."), artifact.to_owned(), version.to_owned()];
    coordinate
        .iter()
        .all(|part| {
            part.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-+".contains(c))
        })
        .then(|| (dir.to_path_buf(), coordinate))
}

/// A POM depending on each coordinate, for `dependency:sources` to fetch the
/// sources of.
fn sources_pom(coordinates: &[[String; 3]]) -> String {
    let mut pom = String::from(
        "<project>\n  <modelVersion>4.0.0</modelVersion>\n  <groupId>javelin</groupId>\n  \
         <artifactId>sources</artifactId>\n  <version>0</version>\n  \
         <packaging>pom</packaging>\n  <dependencies>\n",
    );
    for [group, artifact, version] in coordinates {
        pom.push_str(&format!(
            "    <dependency><groupId>{group}</groupId><artifactId>{artifact}</artifactId>\
             <version>{version}</version></dependency>\n"
        ));
    }
    pom.push_str("  </dependencies>\n</project>\n");
    pom
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("javelin-sources-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn a_maven_repository_path_reads_as_its_coordinates() {
        let jar =
            Path::new("/home/u/.m2/repository/org/mockito/mockito-core/5.2/mockito-core-5.2.jar");
        let (repository, coordinate) = coordinates(jar).expect("coordinates");
        assert_eq!(repository, Path::new("/home/u/.m2/repository"));
        assert_eq!(coordinate, ["org.mockito", "mockito-core", "5.2"]);
    }

    #[test]
    fn a_path_that_is_not_a_maven_repositorys_has_no_coordinates() {
        for jar in [
            // Gradle's cache: a hash directory under the version.
            "/home/u/.gradle/caches/modules-2/files-2.1/org.mockito/mockito-core/5.2/abc123/mockito-core-5.2.jar",
            // Classified.
            "/home/u/.m2/repository/org/x/a/1.0/a-1.0-jdk8.jar",
            // No group.
            "/home/u/.m2/repository/a/1.0/a-1.0.jar",
            // No repository above.
            "/opt/lib/org/x/a/1.0/a-1.0.jar",
            // Not a plain name.
            "/home/u/.m2/repository/org/x<y/a/1.0/a-1.0.jar",
        ] {
            assert_eq!(coordinates(Path::new(jar)), None, "{jar}");
        }
    }

    /// A JAR with no sources beside it is asked for in one run per
    /// repository, with that repository as the local one; a JAR with sources,
    /// or outside a Maven repository, is not asked for.
    #[cfg(unix)]
    #[test]
    fn a_maven_repository_jar_without_sources_is_fetched_into_its_repository() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("local");
        let jar = |path: &str| {
            let jar = dir.join(path);
            std::fs::create_dir_all(jar.parent().expect("dir")).expect("dir");
            std::fs::write(&jar, "zip").expect("jar");
            jar
        };
        let bare = jar("repository/org/x/a/1.0/a-1.0.jar");
        let sourced = jar("repository/org/x/b/2.0/b-2.0.jar");
        jar("repository/org/x/b/2.0/b-2.0-sources.jar");
        let loose = jar("lib/c.jar");
        // The fake writes what it was asked for, and fetches `a`'s sources.
        let maven = dir.join("mvn");
        std::fs::write(
            &maven,
            "#!/bin/sh\necho \"$@\" > args.txt\n\
             for arg in \"$@\"; do case \"$arg\" in -Dmaven.repo.local=*) \
             repo=\"${arg#-Dmaven.repo.local=}\";; esac; done\n\
             touch \"$repo/org/x/a/1.0/a-1.0-sources.jar\"\n",
        )
        .expect("mvn");
        std::fs::set_permissions(&maven, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let work = dir.join("work");
        from_maven_repositories(
            maven.as_os_str(),
            &[bare.clone(), sourced, loose],
            &work,
            &CancellationToken::new(),
            &ChildSlot::default(),
        )
        .expect("ran");
        assert!(bare.with_file_name("a-1.0-sources.jar").exists());
        let pom = std::fs::read_to_string(work.join("sources-0/pom.xml")).expect("pom");
        assert!(
            pom.contains("<artifactId>a</artifactId><version>1.0</version>"),
            "{pom}"
        );
        assert!(!pom.contains("<artifactId>b</artifactId>"), "{pom}");
        let args = std::fs::read_to_string(work.join("sources-0/args.txt")).expect("args");
        assert!(args.contains("dependency:sources"), "{args}");
        assert!(
            args.contains(&format!(
                "-Dmaven.repo.local={}",
                dir.join("repository").display()
            )),
            "{args}"
        );
        assert!(!work.join("sources-1").exists(), "one repository, one run");
    }

    /// Nothing to fetch starts nothing.
    #[test]
    fn with_every_jar_sourced_no_maven_runs() {
        let dir = scratch("none");
        from_maven_repositories(
            OsStr::new("/nonexistent/mvn"),
            &[dir.join("lib/c.jar")],
            &dir,
            &CancellationToken::new(),
            &ChildSlot::default(),
        )
        .expect("ran");
        assert!(!dir.join("sources-0").exists());
    }

    /// The project's own Maven is asked for every dependency's sources, and
    /// its success is the answer: no `mvn` for the generated POM is no
    /// failure, or the fetch would be owed again every session.
    #[cfg(unix)]
    #[test]
    fn a_maven_project_asks_its_own_wrapper_for_sources() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = scratch("project");
        let wrapper = root.join("mvnw");
        std::fs::write(&wrapper, "#!/bin/sh\necho \"$@\" > args.txt\n").expect("mvnw");
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let unsourced = root.join("repository/org/x/a/1.0/a-1.0.jar");
        std::fs::create_dir_all(unsourced.parent().expect("dir")).expect("dir");
        std::fs::write(&unsourced, "zip").expect("jar");
        let ran = download_with(
            OsStr::new("/nonexistent/mvn"),
            &root,
            BuildSystemKind::Maven,
            &[unsourced],
            &root.join("scratch"),
            &CancellationToken::new(),
            &ChildSlot::default(),
        )
        .expect("ran");
        assert!(ran);
        let args = std::fs::read_to_string(root.join("args.txt")).expect("args");
        assert!(
            args.contains("dependency:sources -DfailOnMissingClassifierArtifact=false"),
            "{args}"
        );
    }
}
