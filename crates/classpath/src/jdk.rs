//! Finding a JDK on disk.
//!
//! No classpath is read here, no `.class` file is parsed — this is directory
//! layout and `release`-file text only, so it costs nothing to run on a
//! background thread and needs nothing beyond `std`.

use std::path::{Path, PathBuf};
use std::{env, fs};

/// A JDK found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jdk {
    pub home: PathBuf,
    /// Feature version: 21, 25. `None` when the `release` file is unreadable
    /// or malformed.
    pub version: Option<u32>,
    /// `$home/lib/src.zip`, present only when the file exists.
    pub sources: Option<PathBuf>,
}

/// Search, in order, and take the first home that qualifies as a JDK — one
/// with a `release` file or a `lib/src.zip`:
///
///  1. `hint` — `JdkConfig::home` from the user's config
///  2. `$JAVA_HOME`
///  3. each of `search_paths`
///  4. the built-in locations
///
/// Tiers 3 and 4 take a directory that is itself a JDK home as-is; otherwise
/// it is expanded one level, and within that expansion the highest feature
/// version wins; a home
/// with no readable version loses to any home that has one. A process spawn
/// (`/usr/libexec/java_home`, `java -version`) is deliberately not used: it
/// buys nothing a directory read does not, and it would make this function
/// untestable without a real JDK on the machine running the test.
pub fn discover(hint: Option<&Path>, search_paths: &[PathBuf]) -> Option<Jdk> {
    let java_home = env::var_os("JAVA_HOME").map(PathBuf::from);
    search(
        hint,
        java_home.as_deref(),
        search_paths,
        &built_in_locations(),
    )
}

/// A place to look for a JDK.
///
/// Split out from [`discover`] so tests can drive the search with a
/// fabricated `$JAVA_HOME` and built-in list instead of the real
/// environment — the real built-ins include the developer's own machine
/// (`~/.sdkman/...`), which is exactly the kind of ambient state a
/// `no_jdk_anywhere_answers_none` test cannot tolerate.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Location {
    /// Expand one level; each child is itself the candidate home.
    Children(PathBuf),
    /// Expand one level; join `suffix` onto each child to reach the candidate
    /// home (macOS nests a JDK home under `Contents/Home`).
    ChildrenThen(PathBuf, PathBuf),
    /// Already a specific home; used as-is, never expanded.
    Home(PathBuf),
}

impl Location {
    fn resolve(&self) -> Option<Jdk> {
        match self {
            Location::Children(dir) => resolve_dir(dir),
            Location::ChildrenThen(dir, suffix) => best_in(
                expand(dir)
                    .into_iter()
                    .map(|child| child.join(suffix))
                    .collect(),
            ),
            Location::Home(home) => is_jdk_home(home).then(|| jdk_at(home.clone())),
        }
    }
}

/// A directory that may either be a JDK home itself (a `search_paths` entry
/// pointed straight at one) or a directory of them, one level deep — the
/// same "an entry that is itself a home counts as one" rule the sdkman
/// built-in gets via [`Location::Home`], just for a directory rather than a
/// single fixed path.
fn resolve_dir(dir: &Path) -> Option<Jdk> {
    if is_jdk_home(dir) {
        return Some(jdk_at(dir.to_path_buf()));
    }
    best_in(expand(dir))
}

fn built_in_locations() -> Vec<Location> {
    let mut locations = vec![
        Location::Children(PathBuf::from("/usr/lib/jvm")),
        Location::ChildrenThen(
            PathBuf::from("/Library/Java/JavaVirtualMachines"),
            PathBuf::from("Contents/Home"),
        ),
    ];
    // `env::home_dir` is deprecated (wrong on some old Windows configurations);
    // reading the two environment variables it would have consulted avoids the
    // deprecation without adding a dependency.
    if let Some(home) = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE")) {
        locations.push(Location::Home(
            PathBuf::from(home)
                .join(".sdkman")
                .join("candidates")
                .join("java")
                .join("current"),
        ));
    }
    locations.push(Location::Children(PathBuf::from(r"C:\Program Files\Java")));
    locations
}

fn search(
    hint: Option<&Path>,
    java_home: Option<&Path>,
    search_paths: &[PathBuf],
    built_ins: &[Location],
) -> Option<Jdk> {
    for home in [hint, java_home].into_iter().flatten() {
        if is_jdk_home(home) {
            return Some(jdk_at(home.to_path_buf()));
        }
    }
    for dir in search_paths {
        if let Some(jdk) = resolve_dir(dir) {
            return Some(jdk);
        }
    }
    built_ins.iter().find_map(Location::resolve)
}

/// The immediate children of `dir`, or none when it cannot be read — an
/// absent or unreadable search directory is not a candidate, not a failure.
fn expand(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect()
}

/// The highest-version JDK home among `candidates`, keeping only ones that
/// qualify as a JDK home at all.
fn best_in(candidates: Vec<PathBuf>) -> Option<Jdk> {
    candidates
        .into_iter()
        .filter(|path| is_jdk_home(path))
        .map(jdk_at)
        .max_by_key(|jdk| jdk.version)
}

fn is_jdk_home(home: &Path) -> bool {
    home.join("release").is_file() || home.join("lib").join("src.zip").is_file()
}

fn jdk_at(home: PathBuf) -> Jdk {
    let version = fs::read_to_string(home.join("release"))
        .ok()
        .as_deref()
        .and_then(parse_version);
    let sources = home.join("lib").join("src.zip");
    let sources = sources.is_file().then_some(sources);
    Jdk {
        home,
        version,
        sources,
    }
}

/// `JAVA_VERSION="25.0.2"` -> `25`; `JAVA_VERSION="1.8.0_302"` -> `8`, the
/// legacy scheme where the feature version is the second component.
fn parse_version(release: &str) -> Option<u32> {
    let raw = release
        .lines()
        .find_map(|line| line.strip_prefix("JAVA_VERSION="))?;
    let raw = raw.trim().trim_matches('"');
    let mut parts = raw.split('.');
    let first = parts.next()?;
    if first == "1" {
        parts.next()?.parse().ok()
    } else {
        first.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each test gets its own subdirectory, keyed by name, so parallel tests
    /// never see each other's fixtures.
    fn scratch(name: &str) -> PathBuf {
        let dir = env::temp_dir().join("javelin-classpath-tests").join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write_release(home: &Path, java_version: &str) {
        fs::create_dir_all(home).expect("home dir");
        fs::write(
            home.join("release"),
            format!("JAVA_VERSION=\"{java_version}\"\n"),
        )
        .expect("release file");
    }

    #[test]
    fn the_configured_home_wins_over_java_home() {
        let root = scratch("configured_home_wins");
        let hint_home = root.join("hint");
        let java_home = root.join("java-home");
        write_release(&hint_home, "21");
        write_release(&java_home, "25");

        let jdk = search(Some(&hint_home), Some(&java_home), &[], &[]).expect("a candidate");
        assert_eq!(jdk.home, hint_home);
        assert_eq!(jdk.version, Some(21));
    }

    #[test]
    fn the_highest_version_wins_within_one_search_directory() {
        let root = scratch("highest_version_wins");
        for (name, version) in [("v8", "1.8.0_302"), ("v21", "21.0.4"), ("v17", "17.0.1")] {
            write_release(&root.join(name), version);
        }

        let jdk = search(None, None, std::slice::from_ref(&root), &[]).expect("a candidate");
        assert_eq!(jdk.home, root.join("v21"));
        assert_eq!(jdk.version, Some(21));
    }

    #[test]
    fn a_jdk_without_src_zip_is_still_discovered_with_no_sources() {
        let root = scratch("no_src_zip");
        let home = root.join("jdk");
        write_release(&home, "21");

        let jdk = search(Some(&home), None, &[], &[]).expect("a candidate");
        assert_eq!(jdk.sources, None);
    }

    #[test]
    fn a_release_file_with_a_1_8_version_reads_as_8() {
        let root = scratch("legacy_version");
        let home = root.join("jdk");
        write_release(&home, "1.8.0_302");

        let jdk = search(Some(&home), None, &[], &[]).expect("a candidate");
        assert_eq!(jdk.version, Some(8));
    }

    #[test]
    fn an_unreadable_release_file_yields_a_jdk_with_no_version() {
        let root = scratch("garbage_release");
        let home = root.join("jdk");
        fs::create_dir_all(&home).expect("home dir");
        fs::write(home.join("release"), [0xff, 0xfe, 0x00, 0x01, 0x02]).expect("release file");

        let jdk = search(Some(&home), None, &[], &[]).expect("a candidate");
        assert_eq!(jdk.version, None);
    }

    #[test]
    fn a_search_path_that_is_itself_a_jdk_home_is_discovered() {
        let root = scratch("search_path_is_a_home");
        let home = root.join("company-jdk");
        write_release(&home, "21");

        let jdk = search(None, None, std::slice::from_ref(&home), &[]).expect("a candidate");
        assert_eq!(jdk.home, home);
        assert_eq!(jdk.version, Some(21));
    }

    #[test]
    fn no_jdk_anywhere_answers_none() {
        let root = scratch("nothing_anywhere");
        assert_eq!(search(None, None, &[root], &[]), None);
    }
}
