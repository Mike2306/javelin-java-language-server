//! Persistent workspace state.
//!
//! A type-fact store now exists: `write_types`/`read_types` persist the JDK's
//! `javelin_classpath::TypeFacts` wholesale — its name, kind, type-parameter
//! count, supertypes and the members it declares — written once when the
//! classpath is read and read once at startup; the request path never queries
//! this store.
//! The workspace's own sources are stored beside them in a per-workspace file:
//! what each declares, and every name each writes where a type could be meant,
//! which is what `textDocument/references` reads.
//!
//! Underneath that is the layer this module started with: opening, versioning,
//! and discarding the on-disk cache. That part was worth having first because it
//! decides the shape of everything stored later, and because "the cache from the
//! last session is unreadable" must be a recoverable event, not a crash.
//!
//! SQLite is the storage engine. It is boring, transactional, ships as a single
//! file, and survives being killed mid-write — all of which matter more for a
//! background indexer than raw throughput. See `docs/adr/0002-persistence.md`.

mod types;
mod walk;
mod workspace;

use std::path::Path;

pub use walk::{
    Found, LombokLog, Survey, admits, is_build_output, is_generated_source, is_processor_output,
    java_sources, java_sources_surveyed, reaches_processor_output, within,
};
pub use workspace::{
    MemberRef, REFS_WITH_JDK, REFS_WITHOUT_WORLD, SourceChange, Walked, WorkspaceFile,
    WorkspaceIndex, WorkspaceMember, WorkspaceMemberRef, WorkspaceOccurrence, WorkspaceType,
    decode_source, refs_settled,
};

use javelin_core::{Error, Result};
use rusqlite::Connection;

/// Bumped whenever the on-disk layout changes.
///
/// A mismatch discards the cache rather than attempting a migration: the data is
/// derived from source that is still on disk, so rebuilding is always correct
/// and always cheaper than maintaining migration code.
///
/// Bumped to 2 for the `types`/`supertypes` tables added alongside
/// `write_types`, to 3 for dropping the `types_simple` index nothing read, to
/// 4 for the `members` table, and to 5 for `types.source` and
/// `members.name_start`/`name_end`. That bump is load-bearing rather than
/// tidy, same as 4's: a version-4 cache holds a `source` of `""` and spans of
/// `0, 0` for every row, indistinguishable from a real entry whose name
/// starts at byte zero, so a version-4 cache read under this schema would
/// send a definition request to the wrong byte instead of refusing it.
///
/// Bumped to 6 for the `source_files`/`source_types`/`source_supertypes`
/// tables — the workspace's own sources. One constant covers every cache file
/// this crate opens, so that bump also discards the JDK's fact cache and costs
/// one eleven-second background re-read per machine. Deliberate: the cache is
/// derived data, the re-read blocks nothing, and a second versioning scheme is
/// more code than the cost it would avoid.
///
/// Bumped to 8 for `source_imports` and `source_occurrences`, the reference
/// index. Load-bearing like 5: a version-7 cache holds files the walk would
/// skip as unchanged and no occurrences for them, so every reference into
/// them would be silently missing until each was edited.
///
/// Bumped to 9 for `source_members`. Load-bearing for the reason 8 was: a
/// version-8 cache would keep every unchanged file and no members for it.
///
/// Bumped to 10 for `source_members.is_private` and `type_params`, which the
/// member closure needs to refuse a type variable and a private member.
///
/// Bumped to 11 for `source_member_refs` and `source_member_deps`: a version-10
/// cache would skip every unchanged file and never resolve its member
/// references.
///
/// Bumped to 12 for `source_files.member_refs`, which says whether a file's
/// member references were stored and whether against the JDK: without it a
/// session killed before its resolution pass finished leaves files whose
/// references the next walk, finding them unchanged, never resolves.
///
/// Bumped to 13 for `source_members.decl_start` and `decl_end`, which the call
/// hierarchy groups a use under: a version-12 cache would keep every
/// unchanged file with no declaration span for any of its members.
///
/// Bumped to 14 for `types.members_complete` and `source_files.parsed_cleanly`,
/// which say whether a type's member list is all of it: a member diagnostic
/// claims a name is absent, and a version-13 cache would claim it of a type
/// whose source parsed with errors.
///
/// Bumped to 15 for type parameter names, a supertype's type arguments and
/// generic member types: a version-14 cache would answer `List<String>.get`
/// with an unresolved `E` until the JDK was re-read.
///
/// Bumped to 16 for `members.is_abstract`, which a lambda parameter is typed
/// through: a version-15 cache knows no functional interface's method.
///
/// Bumped to 17 for `types.name_start` and `name_end`, the type's own name in
/// its `src.zip` entry, which a jump to the type lands on: a version-16 cache
/// would site no JDK type until the JDK was re-read.
///
/// Bumped to 18 for `source_types.synthetic` and `source_members.synthetic`,
/// the members and the builder class Lombok generates: a version-17 cache
/// would keep every unchanged file with none of them.
///
/// Bumped to 19 for `members.param_names` and `source_members.param_names`,
/// a parameter's name beside its type for hover and inlay hints: a
/// version-18 cache would name no parameter until the JDK was re-read.
///
/// Bumped to 20 for a declaration's Javadoc span beside its name's —
/// `types`, `members`, `source_types` and `source_members` — which hover
/// reads the comment through: a version-19 cache has none.
///
/// Bumped to 21 for `members.own_type_params`, a generic method's own type
/// parameters, which a call's arguments bind: a version-20 cache would infer
/// no generic method's result until the JDK was re-read.
///
/// Bumped to 22 for `source_occurrences` rows naming a qualified name's
/// segments by their dotted text: a version-21 cache would list no
/// `java.util.List` among `List`'s references until each file was re-read.
///
/// Bumped to 23 for `members.is_public` and `source_members.is_public`, which
/// most-specific selection asks of its winner, and for a C-style array
/// declarator's `[]` now being part of a written type.
///
/// Bumped to 24 because an unqualified call inside a nested class now
/// resolves through the class around it: a version-23 cache's
/// `source_member_refs` would miss every such call until each file was
/// resolved again.
///
/// Bumped to 25 because a workspace type that only its own build's JAR also
/// holds, or whose file statically imports a name its supertypes do not
/// spell, now has a whole member list, which most-specific selection and the
/// enclosing-class search read: a version-24 cache's `source_member_refs`
/// would miss the calls they now resolve until each file was resolved again.
///
/// Bumped to 26 because Lombok's `canEqual` is now generated, and a member
/// type a static import may name is no longer resolved through the tiers.
///
/// Bumped to 27 for `source_types.lombok`: whether Lombok writes into a type
/// and whether everything it writes there is generated.
///
/// Bumped to 28 for an enum's `values()` and `valueOf` and a record's
/// accessors, stored as generated members, and for a JDK enum's or record's
/// list counting as whole.
///
/// Bumped to 29 because a statically imported member is now resolved, so a
/// file's `source_member_refs` hold uses a version-28 cache missed.
///
/// Bumped to 30 for `source_types.type_params`: a workspace type's own type
/// parameter names, which its members' types and its supertype edges are
/// now read in.
/// Bumped to 31 for the field a Lombok log annotation writes, stored among
/// a type's generated members.
///
/// Bumped to 32 because a for-each `var` and a class literal are now typed,
/// and a workspace member's nested type is refused where an inherited one
/// may shadow it, so a version-31 cache's `source_member_refs` hold the
/// wrong set until each file is resolved again.
///
/// Bumped to 33 because an annotation's element key is now a member
/// selection and no longer a plain name, so a version-32 cache's
/// `source_member_refs` and `source_occurrences` are the wrong set.
///
/// Bumped to 34 for constructors: the JDK's are stored as `members` rows of
/// the constructor kind, a workspace type's as `source_members` rows, and a
/// `new` that selects one is a member reference.
///
/// Bumped to 35 because a method reference now types the generic call it is
/// passed to, so a version-34 cache's `source_member_refs` hold the wrong set.
///
/// Bumped to 36 because a block lambda's `return`s now type the generic call
/// it is passed to, for the same reason.
///
/// Bumped to 37 because a generic call passed for a parameter is now inferred
/// toward it, for the same reason.
///
/// Bumped to 38 because the worker records a bare name a supertype's field
/// or a static import means, so a version-37 cache's `source_member_refs`
/// lack them. And to 39 because a local one `case` group declares is in
/// scope in the next, so a version-38 cache recorded a field there.
/// To 40 because the worker records a call into a JAR member, or a JDK
/// member of a type the workspace extends, under the member's key.
/// To 41 because an enum's written constructors are stored, and the worker
/// records `this(…)`, `super(…)` and an enum constant as constructor calls.
/// To 42 because `@SuperBuilder` and `@Singular` generate members and
/// builder types, with supertypes and type parameters, a version-41 cache
/// does not hold.
/// To 43 because the worker records a `case` label under an enum selector as
/// a use of the enum's constant.
/// To 44 because a JDK argument now excludes a JAR or workspace parameter
/// type, so the worker picks overloads, slf4j's `info(String, Object, Object)`
/// among them, it refused before.
/// To 45 because a generic type's `@Builder` is generated now, and a call's
/// written type arguments bind its method's own variables, so the worker
/// follows chains it stopped at.
/// To 46 because a type's fact says whether it is public, which the
/// add-import quick fix offers only those of.
pub const SCHEMA_VERSION: i32 = 46;

/// A handle to one persistent cache file.
///
/// Not necessarily a workspace's. The JDK type facts are stored at
/// `cache_root/jdk.sqlite`, keyed by JDK identity and deliberately shared
/// across every workspace on the machine, because one `src.zip` read should
/// warm all of them. A per-workspace database is what
/// `javelin_core::paths::workspace_cache_dir` is for, and nothing opens one
/// yet.
#[derive(Debug)]
pub struct CacheDb {
    connection: Connection,
    /// Directories a `lombok.config` renames generated members below — see
    /// [`Survey::lombok_renamed`]. What Lombok generates for a file below one
    /// is stored and never published.
    lombok_renamed: Vec<std::path::PathBuf>,
    /// What `lombok.config` files say a log annotation's field is called —
    /// see [`Survey::lombok_log`]. Applied when an index is published, since
    /// a config can change without any source file changing.
    lombok_log: std::collections::BTreeMap<std::path::PathBuf, LombokLog>,
}

impl CacheDb {
    /// Open (or create) the cache at `path`, rebuilding it if it was written by
    /// an incompatible version of Javelin.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| Error::io(parent, source))?;
        }
        let mut db = Self::connect(path)?;
        if db.stored_schema_version()? != SCHEMA_VERSION {
            drop(db);
            std::fs::remove_file(path)
                .or_else(ignore_missing)
                .map_err(|source| Error::io(path, source))?;
            db = Self::connect(path)?;
        }
        db.initialize()?;
        Ok(db)
    }

    /// An in-memory cache, for tests and for `--no-persist` runs.
    pub fn in_memory() -> Result<Self> {
        let db = Self {
            connection: Connection::open_in_memory().map_err(to_index_error)?,
            lombok_renamed: Vec::new(),
            lombok_log: std::collections::BTreeMap::new(),
        };
        db.initialize()?;
        Ok(db)
    }

    fn connect(path: &Path) -> Result<Self> {
        let connection = Connection::open(path).map_err(to_index_error)?;
        // WAL keeps background writers from blocking readers on the request path.
        // NORMAL trades a crash-window of derived data for far fewer fsyncs; the
        // worst case is a rebuild, which the schema check already handles.
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .and_then(|()| connection.pragma_update(None, "synchronous", "NORMAL"))
            .map_err(to_index_error)?;
        Ok(Self {
            connection,
            lombok_renamed: Vec::new(),
            lombok_log: std::collections::BTreeMap::new(),
        })
    }

    /// The directories below which what Lombok generates is not believed,
    /// for every publish from here on.
    pub fn set_lombok_renamed(
        &mut self,
        directories: impl IntoIterator<Item = std::path::PathBuf>,
    ) {
        self.lombok_renamed = directories.into_iter().collect();
    }

    /// What `lombok.config` files say a log annotation's field is called,
    /// for every publish from here on.
    pub fn set_lombok_log(
        &mut self,
        configs: std::collections::BTreeMap<std::path::PathBuf, LombokLog>,
    ) {
        self.lombok_log = configs;
    }

    /// Whether `path` is below a directory a `lombok.config` renames in.
    pub(crate) fn lombok_renamed_at(&self, path: &Path) -> bool {
        self.lombok_renamed
            .iter()
            .any(|directory| path.starts_with(directory))
    }

    fn stored_schema_version(&self) -> Result<i32> {
        self.connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(to_index_error)
    }

    fn initialize(&self) -> Result<()> {
        self.connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
            )
            .map_err(to_index_error)?;
        types::initialize(&self.connection)?;
        workspace::initialize(&self.connection)?;
        self.connection
            .pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(to_index_error)
    }

    pub fn set_metadata(&self, key: &str, value: &str) -> Result<()> {
        self.connection
            .execute(
                "INSERT INTO metadata (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                (key, value),
            )
            .map(|_| ())
            .map_err(to_index_error)
    }

    pub fn metadata(&self, key: &str) -> Result<Option<String>> {
        self.connection
            .query_row("SELECT value FROM metadata WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .map(Some)
            .or_else(|err| match err {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(to_index_error(other)),
            })
    }

    /// Escape hatch for the index implementations that land in phase 3.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }
}

fn to_index_error(err: rusqlite::Error) -> Error {
    Error::Index {
        message: err.to_string(),
    }
}

fn ignore_missing(err: std::io::Error) -> std::io::Result<()> {
    if err.kind() == std::io::ErrorKind::NotFound {
        Ok(())
    } else {
        Err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("javelin-index-tests");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(format!("{name}.sqlite"));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn metadata_round_trips_in_memory() {
        let db = CacheDb::in_memory().expect("open");
        assert_eq!(db.metadata("jdk").expect("read"), None);
        db.set_metadata("jdk", "21").expect("write");
        db.set_metadata("jdk", "23").expect("overwrite");
        assert_eq!(db.metadata("jdk").expect("read"), Some("23".to_owned()));
    }

    #[test]
    fn a_cache_survives_being_reopened() {
        let path = scratch("reopen");
        CacheDb::open(&path)
            .expect("create")
            .set_metadata("k", "v")
            .expect("write");
        let reopened = CacheDb::open(&path).expect("reopen");
        assert_eq!(reopened.metadata("k").expect("read"), Some("v".to_owned()));
    }

    #[test]
    fn a_cache_from_an_incompatible_version_is_discarded_not_fatal() {
        // Both directions, because only one of them is hypothetical: the
        // predecessor version is what every machine that has already run this
        // server has on disk, and a cache from it must be rebuilt rather than
        // read as if the layout had not changed under it.
        for faked in [SCHEMA_VERSION + 99, SCHEMA_VERSION - 1] {
            let path = scratch(&format!("stale-{faked}"));
            {
                let db = CacheDb::open(&path).expect("create");
                db.set_metadata("k", "stale").expect("write");
                db.connection()
                    .pragma_update(None, "user_version", faked)
                    .expect("fake an incompatible schema");
            }
            let rebuilt = CacheDb::open(&path).expect("reopening must recover, not fail");
            assert_eq!(
                rebuilt.metadata("k").expect("read"),
                None,
                "stale rows must be gone"
            );
        }
    }
}
