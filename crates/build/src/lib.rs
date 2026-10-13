//! Build-system integration.
//!
//! Resolving a real Maven or Gradle project is slow, network-dependent, and
//! frequently broken in ways Javelin cannot fix. So it is treated as a
//! *background enrichment*: the editor is fully usable before this crate has
//! said anything, and the classpath it eventually produces upgrades the answers
//! rather than unlocking them.
//!
//! Two things: guessing the build system from marker files, cheap enough for
//! `initialize`, and asking it for the dependency classpath, which is not and
//! runs on a background thread — see [`resolve_classpath`].

use std::path::Path;

mod classpath;
mod sources;

pub use classpath::{
    BuildHints, ChildSlot, Module, Resolved, build_files_key, build_hints, has_build_files,
    is_build_path, kill_child, parse_gradle_output, parse_maven_output, resolve_classpath,
    resolve_classpath_reporting,
};
pub use sources::download_sources;

/// Which build system a workspace appears to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildSystemKind {
    Maven,
    Gradle,
}

impl BuildSystemKind {
    pub fn name(self) -> &'static str {
        match self {
            BuildSystemKind::Maven => "maven",
            BuildSystemKind::Gradle => "gradle",
        }
    }
}

/// Marker files, most specific first. Gradle is checked before Maven because a
/// polyglot repository that has both is almost always Gradle-driven.
const MARKERS: &[(&str, BuildSystemKind)] = &[
    ("settings.gradle.kts", BuildSystemKind::Gradle),
    ("settings.gradle", BuildSystemKind::Gradle),
    ("build.gradle.kts", BuildSystemKind::Gradle),
    ("build.gradle", BuildSystemKind::Gradle),
    ("pom.xml", BuildSystemKind::Maven),
];

/// Guess the build system from marker files in the workspace root.
///
/// This is a filename check and nothing more: it must stay cheap enough to run
/// during `initialize` without measurably delaying it. Returning `None` is fine
/// and means Javelin treats the workspace as loose sources.
pub fn detect(workspace_root: &Path) -> Option<BuildSystemKind> {
    MARKERS
        .iter()
        .find(|(marker, _)| workspace_root.join(marker).is_file())
        .map(|(_, kind)| *kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str, files: &[&str]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("javelin-build-detect-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        for file in files {
            std::fs::write(dir.join(file), "").expect("marker file");
        }
        dir
    }

    #[test]
    fn detects_maven() {
        assert_eq!(
            detect(&scratch("maven", &["pom.xml"])),
            Some(BuildSystemKind::Maven)
        );
    }

    #[test]
    fn detects_gradle() {
        assert_eq!(
            detect(&scratch("gradle", &["build.gradle.kts"])),
            Some(BuildSystemKind::Gradle)
        );
    }

    #[test]
    fn a_mixed_repository_is_treated_as_gradle() {
        let dir = scratch("mixed", &["pom.xml", "settings.gradle"]);
        assert_eq!(detect(&dir), Some(BuildSystemKind::Gradle));
    }

    #[test]
    fn loose_sources_are_not_an_error() {
        assert_eq!(detect(&scratch("loose", &[])), None);
    }
}
