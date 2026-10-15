//! One entry of the JDK's `src.zip`, on disk, so a client has somewhere to
//! jump to.
//!
//! No editor opens a path *inside* a zip, and a custom scheme that named one
//! would need extension support neither the Zed nor the VS Code skeleton has.
//! So a jump into the JDK's own sources means extracting the single entry it
//! lands in and pointing the client at a `file:` URI.
//!
//! Extraction is per entry and on demand. A session that never jumps into the
//! JDK writes nothing, and one that jumps into `String.java` twice opens the
//! archive once — the second ask is a file read.

use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use javelin_classpath::SourceArchive;

/// The JDK's own sources, extracted one file at a time so a client has
/// somewhere to jump to.
///
/// ponytail: the extracted copy is an ordinary writable file, so an edit to it
/// is compiled by nothing and is overwritten by the next session that extracts
/// the entry. Marking it read-only after the rename would say so out loud; it
/// is per-platform work that a jump into a JDK does not yet justify.
#[derive(Debug)]
pub(crate) struct JdkSources {
    src_zip: PathBuf,
    /// `<cache root>/jdk-sources/<version>-<key of the src.zip path>`.
    ///
    /// Under the cache root rather than a workspace directory, for the reason
    /// the fact cache is: one JDK read should serve every project on the
    /// machine.
    ///
    /// The version is in the key because the fact cache is keyed by it too. A
    /// JDK replaced in place at the same home — a new feature version, or a
    /// patch level whose `src.zip` has another modification time or size, both
    /// of which `classpath::identity` puts in the version — misses the fact
    /// cache and gets its member offsets re-read from the new archive; an
    /// extraction directory that did not move with it would answer those new
    /// offsets with the old file's text, which is a jump landing on the wrong
    /// declaration rather than a jump that does not happen.
    dir: PathBuf,
    /// Opened on the first miss and kept: the central directory of a JDK's
    /// `src.zip` is tens of thousands of entries.
    archive: Option<SourceArchive>,
    /// What this session has extracted itself, and so may read back from disk.
    /// Empty and consulted only when the user has turned persistence off —
    /// see [`JdkSources::new`].
    extracted: HashSet<String>,
    /// Whether a file a *previous* session left behind may be believed.
    trusts_disk: bool,
    /// The last few entries' text, so hovering one method after another in
    /// one file reads it once: hover asks on every rest of the mouse.
    recent: std::collections::VecDeque<(String, String)>,
}

/// How many extracted entries' text [`JdkSources`] keeps.
const RECENT: usize = 8;

impl JdkSources {
    /// `persist` is the user saying "do not reuse anything from a previous
    /// session", and it governs what may be *read back*, not whether anything
    /// is written: the extracted file is how a client is given something to
    /// open at all, so refusing to write it would refuse the feature rather
    /// than refuse a cache. With it off, the first ask for an entry in this
    /// session extracts over whatever is there and the rest of the session
    /// reuses that.
    pub(crate) fn new(cache_root: &Path, src_zip: PathBuf, version: &str, persist: bool) -> Self {
        let key = format!("{version}-{}", javelin_core::paths::stable_key(&src_zip));
        let dir = cache_root.join("jdk-sources").join(key);
        Self {
            src_zip,
            dir,
            archive: None,
            extracted: HashSet::new(),
            trusts_disk: persist,
            recent: std::collections::VecDeque::new(),
        }
    }

    /// The extracted file's path and its text, extracting it if this is the
    /// first ask. `None` for anything that did not work — a refused entry
    /// name, an archive that will not open, an entry that is not there, a
    /// write that failed. Every one of those is a missing jump and a `debug`
    /// line, never an error on the wire.
    pub(crate) fn materialize(&mut self, entry: &str, max_bytes: u64) -> Option<(PathBuf, String)> {
        let destination = self.destination(entry)?;
        if let Some((_, text)) = self.recent.iter().find(|(seen, _)| seen == entry) {
            return Some((destination, text.clone()));
        }
        let (destination, text) = self.extract(entry, destination, max_bytes)?;
        if self.recent.len() == RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back((entry.to_owned(), text.clone()));
        Some((destination, text))
    }

    fn extract(
        &mut self,
        entry: &str,
        destination: PathBuf,
        max_bytes: u64,
    ) -> Option<(PathBuf, String)> {
        // Reusing what is on disk is what makes the second jump into one file
        // cost a file read rather than a decompression.
        if (self.trusts_disk || self.extracted.contains(entry))
            && let Ok(text) = fs::read_to_string(&destination)
        {
            return Some((destination, text));
        }
        let text = self.read(entry, max_bytes)?;
        write_atomically(&destination, &text)
            .inspect_err(|err| {
                tracing::debug!(
                    path = %destination.display(),
                    %err,
                    "jdk sources: could not write the extracted entry"
                );
            })
            .ok()?;
        if !self.trusts_disk {
            self.extracted.insert(entry.to_owned());
        }
        Some((destination, text))
    }

    fn read(&mut self, entry: &str, max_bytes: u64) -> Option<String> {
        if self.archive.is_none() {
            match SourceArchive::open(&self.src_zip) {
                Ok(archive) => self.archive = Some(archive),
                Err(err) => {
                    tracing::debug!(
                        src_zip = %self.src_zip.display(),
                        %err,
                        "jdk sources: archive will not open"
                    );
                    return None;
                }
            }
        }
        self.archive
            .as_mut()?
            .read(entry, max_bytes)
            .inspect_err(|err| tracing::debug!(entry, %err, "jdk sources: entry unreadable"))
            .ok()
    }

    /// A dependency's `-sources.jar`, extracted the same way under
    /// `<cache root>/jar-sources/<time and size>-<key of its path>`: keyed by
    /// what the JAR store keyed the grafted offsets by, so a sources JAR
    /// replaced in place is extracted fresh rather than answered from the
    /// old one's text.
    pub(crate) fn for_sources_jar(cache_root: &Path, archive: PathBuf, persist: bool) -> Self {
        let stamp = fs::metadata(&archive)
            .map(|metadata| {
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |since| since.as_nanos());
                format!("{modified}-{}", metadata.len())
            })
            .unwrap_or_default();
        let key = format!("{stamp}-{}", javelin_core::paths::stable_key(&archive));
        Self {
            dir: cache_root.join("jar-sources").join(key),
            src_zip: archive,
            archive: None,
            extracted: HashSet::new(),
            trusts_disk: persist,
            recent: std::collections::VecDeque::new(),
        }
    }

    /// Where `entry` lands under [`Self::dir`], or `None` for a name that
    /// would not stay there.
    ///
    /// An entry name is input from a file nobody here wrote, and a `..` in one
    /// is the standard way an extractor is talked into writing outside the
    /// directory it meant to. Refused rather than sanitised: every entry a JDK
    /// holds is a plain relative path, so anything else is not a name worth
    /// repairing into one.
    fn destination(&self, entry: &str) -> Option<PathBuf> {
        // Zip names separate with `/` (APPNOTE 4.4.17.1), so a backslash is
        // never part of a real one — and it is how `C:\..` reaches a platform
        // that reads it as both a drive prefix and a separator. Refusing it
        // everywhere keeps the two platforms answering the same thing.
        if entry.contains('\\') {
            tracing::debug!(
                entry,
                "jdk sources: refusing an entry name with a backslash"
            );
            return None;
        }
        let mut destination = self.dir.clone();
        for component in Path::new(entry).components() {
            let Component::Normal(part) = component else {
                tracing::debug!(
                    entry,
                    "jdk sources: refusing an entry name that is not relative"
                );
                return None;
            };
            destination.push(part);
        }
        // Equality catches the empty name, which pushed nothing. The prefix
        // check is the belt to that pair of braces: a root or a drive prefix
        // replaces the path `push` was called on rather than extending it.
        (destination != self.dir && destination.starts_with(&self.dir)).then_some(destination)
    }
}

/// Write through a temporary name in the same directory and rename, so a
/// process killed mid-write leaves a stray temporary rather than a truncated
/// file that a later session reads back as the JDK's source.
fn write_atomically(destination: &Path, text: &str) -> std::io::Result<()> {
    let parent = destination.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent directory")
    })?;
    fs::create_dir_all(parent)?;
    let name = destination
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let temporary = parent.join(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&temporary, text)?;
    fs::rename(&temporary, destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Deliberately not plain ASCII with Unix line endings: what a client
    /// opens has to be the entry's bytes, and a JDK's sources hold both a
    /// non-ASCII character and, in places, a CRLF.
    const WIDGET: (&str, &str) = (
        "java.base/com/example/Widget.java",
        "package com.example;\r\npublic class Widget {\n    String größe = \"m²\";\n}\n",
    );

    const NO_LIMIT: u64 = u64::MAX;

    /// A feature release, as [`crate::classpath`] writes one into the key.
    const VERSION: &str = "21";

    /// The ordinary case: a session allowed to believe what a previous one
    /// extracted.
    fn persistent(cache_root: &Path, src_zip: PathBuf) -> JdkSources {
        JdkSources::new(cache_root, src_zip, VERSION, true)
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("javelin-server-tests")
            .join("jdk-sources")
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Build a `src.zip` at `dir/src.zip`, stored uncompressed — nothing here
    /// cares which method wrote the archive it reads back.
    fn write_zip(dir: &Path, entries: &[(&str, &str)]) -> PathBuf {
        let path = dir.join("src.zip");
        let file = fs::File::create(&path).expect("zip file");
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, contents) in entries {
            writer.start_file(*name, options).expect("start entry");
            writer.write_all(contents.as_bytes()).expect("write entry");
        }
        writer.finish().expect("finish zip");
        path
    }

    /// Hovering one member after another in a file reads it once: the last
    /// few entries' text is kept, so the second ask touches no disk at all.
    #[test]
    fn a_recent_entry_is_answered_without_reading_anything() {
        let dir = scratch("recent");
        let zip_path = write_zip(&dir, &[WIDGET]);
        let cache = dir.join("cache");
        let mut sources = persistent(&cache, zip_path);
        let (path, first) = sources.materialize(WIDGET.0, NO_LIMIT).expect("extracted");
        // A disk read would see this; the kept text does not.
        fs::write(&path, "changed").expect("overwrite the extracted copy");
        let (_, again) = sources
            .materialize(WIDGET.0, NO_LIMIT)
            .expect("from memory");
        assert_eq!(again, first);
    }

    #[test]
    fn an_entry_is_extracted_once_and_read_from_disk_after_that() {
        let dir = scratch("extract_once");
        let zip_path = write_zip(&dir, &[WIDGET]);
        let cache = dir.join("cache");

        let mut sources = persistent(&cache, zip_path.clone());
        let first = sources.materialize(WIDGET.0, NO_LIMIT).expect("first ask");

        // The archive is the only thing the first ask needed, so taking it
        // away is how the second one proves it did not go back to it.
        drop(sources);
        fs::remove_file(&zip_path).expect("remove the archive");
        let mut sources = persistent(&cache, zip_path);

        assert_eq!(sources.materialize(WIDGET.0, NO_LIMIT), Some(first));
    }

    /// The fact cache is keyed by the JDK's version, so a JDK replaced by a
    /// different one has its member offsets re-read. An extraction directory
    /// that did not move with it would answer those offsets with the old
    /// file's text — a jump landing somewhere wrong, which is worse than one
    /// that does not happen. That a patch-level replacement moves the version
    /// is `classpath::identity`'s, tested there.
    #[test]
    fn a_jdk_replaced_at_the_same_home_does_not_reuse_the_old_versions_files() {
        let dir = scratch("version_key");
        let cache = dir.join("cache");
        let zip_path = write_zip(&dir, &[WIDGET]);
        let mut old = persistent(&cache, zip_path.clone());
        old.materialize(WIDGET.0, NO_LIMIT).expect("the old JDK");

        // Same home, same entry name, different sources.
        let replaced = "package com.example;\npublic class Widget { int added; }\n";
        let zip_path = write_zip(&dir, &[(WIDGET.0, replaced)]);
        let mut new = JdkSources::new(&cache, zip_path, "22", true);

        let (_, text) = new.materialize(WIDGET.0, NO_LIMIT).expect("the new JDK");
        assert_eq!(text, replaced);
    }

    /// `index.persist` off is the user saying a previous session's leftovers
    /// are not to be believed — which is exactly what they reach for after
    /// replacing a JDK. It governs what may be read back, not whether anything
    /// is written: without the write there is no file for a client to open.
    #[test]
    fn with_persistence_off_a_file_a_previous_session_wrote_is_extracted_again() {
        let dir = scratch("no_persist");
        let zip_path = write_zip(&dir, &[WIDGET]);
        let mut sources = JdkSources::new(&dir.join("cache"), zip_path.clone(), VERSION, false);

        // Stand in for the previous session: a file already at the
        // destination, holding something the archive does not.
        let destination = sources.destination(WIDGET.0).expect("a destination");
        write_atomically(&destination, "class Stale {}\n").expect("the stale file");

        let (path, text) = sources.materialize(WIDGET.0, NO_LIMIT).expect("extracted");
        assert_eq!(path, destination);
        assert_eq!(text, WIDGET.1);

        // The second ask in the same session reuses what this session wrote,
        // so persistence costs the archive one read and not one per ask.
        fs::remove_file(&zip_path).expect("remove the archive");
        assert_eq!(
            sources.materialize(WIDGET.0, NO_LIMIT),
            Some((path, text)),
            "this session's own extraction is reusable"
        );
    }

    #[test]
    fn an_entry_name_that_climbs_out_of_the_cache_directory_is_refused() {
        let dir = scratch("zip_slip");
        let escape = "../escaped.java";
        let zip_path = write_zip(&dir, &[(escape, "class Escaped {}\n"), WIDGET]);
        let cache = dir.join("cache");
        let mut sources = persistent(&cache, zip_path);
        // Where a successful escape would land: one level out of the
        // extraction directory, which is several below `dir`.
        let escaped = sources
            .dir
            .parent()
            .expect("the extraction directory has a parent")
            .join("escaped.java");

        for name in [escape, "/absolute.java", "C:\\windows.java", "", "."] {
            // Both halves: the name is refused before the archive is asked,
            // and the ask answers nothing. Without the first, a missing entry
            // would make the second true for the wrong reason.
            assert_eq!(sources.destination(name), None, "{name} must be refused");
            assert_eq!(
                sources.materialize(name, NO_LIMIT),
                None,
                "{name} must not be extracted"
            );
        }
        assert!(
            !escaped.exists(),
            "nothing may be written outside the extraction directory"
        );
    }

    #[test]
    fn an_entry_the_archive_does_not_hold_answers_none() {
        let dir = scratch("missing_entry");
        let zip_path = write_zip(&dir, &[WIDGET]);
        let mut sources = persistent(&dir.join("cache"), zip_path);

        assert_eq!(
            sources.materialize("java.base/com/example/Nowhere.java", NO_LIMIT),
            None
        );
    }

    #[test]
    fn an_archive_that_is_not_there_answers_none_rather_than_failing() {
        let dir = scratch("no_archive");
        let mut sources = persistent(&dir.join("cache"), dir.join("src.zip"));

        assert_eq!(sources.materialize(WIDGET.0, NO_LIMIT), None);
    }

    #[test]
    fn the_extracted_text_is_byte_for_byte_the_entry() {
        let dir = scratch("byte_for_byte");
        let zip_path = write_zip(&dir, &[WIDGET]);
        let mut sources = persistent(&dir.join("cache"), zip_path);

        let (path, text) = sources.materialize(WIDGET.0, NO_LIMIT).expect("extracted");

        assert_eq!(text, WIDGET.1);
        assert_eq!(fs::read_to_string(&path).expect("the file"), WIDGET.1);
    }
}
