//! Storing the JDK's type facts: [`TypeFacts`] rows, the [`MemberFacts`] each
//! type declares, and the schema behind them.
//!
//! This is a bulk-write-once, bulk-read-once store, not a query engine. The
//! request path never touches it — `javelin-server` loads `read_types`'s
//! result into an immutable [`javelin_classpath::TypeWorld`] and shares that
//! as an `Arc`, once, at startup. So there is no per-`fqn` lookup here and no
//! secondary index at all: an index nothing queries is write cost on every
//! one of ~24,000 rows for no read, and it comes back with its first reader.
//! Members are read in one ordered pass and grouped by `fqn` for the same
//! reason supertypes are, which is what keeps that still true of them.

use std::collections::{HashMap, HashSet};

use javelin_classpath::{DeclKind, MemberFacts, MemberType, TypeFacts};
use javelin_core::Result;
use rusqlite::Connection;

use crate::{CacheDb, to_index_error};

pub(crate) fn initialize(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS types (
                 fqn         TEXT PRIMARY KEY,
                 simple      TEXT NOT NULL,
                 kind        INTEGER NOT NULL,
                 type_params TEXT NOT NULL,
                 complete    INTEGER NOT NULL,
                 members_complete INTEGER NOT NULL,
                 is_public   INTEGER NOT NULL,
                 source      TEXT NOT NULL,
                 name_start  INTEGER NOT NULL,
                 name_end    INTEGER NOT NULL,
                 doc_start   INTEGER,
                 doc_end     INTEGER
             ) STRICT;
             CREATE TABLE IF NOT EXISTS supertypes (
                 fqn       TEXT NOT NULL,
                 super_fqn TEXT NOT NULL,
                 arguments TEXT NOT NULL,
                 PRIMARY KEY (fqn, super_fqn)
             ) STRICT, WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS members (
                 fqn        TEXT NOT NULL,
                 position   INTEGER NOT NULL,
                 name       TEXT NOT NULL,
                 kind       INTEGER NOT NULL,
                 is_static  INTEGER NOT NULL,
                 is_abstract INTEGER NOT NULL,
                 is_public  INTEGER NOT NULL,
                 declared   TEXT NOT NULL,
                 params     TEXT NOT NULL,
                 variadic   INTEGER NOT NULL,
                 name_start INTEGER NOT NULL,
                 name_end   INTEGER NOT NULL,
                 param_names TEXT NOT NULL,
                 doc_start  INTEGER,
                 doc_end    INTEGER,
                 own_type_params TEXT NOT NULL,
                 PRIMARY KEY (fqn, position)
             ) STRICT, WITHOUT ROWID;",
        )
        .map_err(to_index_error)
}

/// A stable on-disk code for a [`DeclKind`] variant.
///
/// Deliberately not `as i64` on the enum or `std::mem::discriminant`: either
/// would derive the number from declaration order, so inserting a new variant
/// would silently reinterpret every row a previous version already wrote.
/// Writing the mapping out by hand makes that impossible — a new variant needs
/// a new arm here, chosen, not inherited.
pub(crate) fn kind_code(kind: DeclKind) -> i64 {
    match kind {
        DeclKind::Class => 0,
        DeclKind::Interface => 1,
        DeclKind::Enum => 2,
        DeclKind::Record => 3,
        DeclKind::AnnotationType => 4,
        DeclKind::AnonymousClass => 5,
        DeclKind::Field => 6,
        DeclKind::Method => 7,
        DeclKind::Constructor => 8,
        DeclKind::StaticInitializer => 9,
        DeclKind::InstanceInitializer => 10,
        DeclKind::EnumConstant => 11,
        DeclKind::RecordComponent => 12,
        DeclKind::AnnotationElement => 13,
        DeclKind::Parameter => 14,
        DeclKind::LocalVariable => 15,
    }
}

/// The inverse of [`kind_code`]. `None` for a code this build's [`DeclKind`]
/// has no variant for — a row a future Javelin wrote — so a reader can skip
/// that row instead of panicking or misreading it as some other kind.
pub(crate) fn from_kind_code(code: i64) -> Option<DeclKind> {
    match code {
        0 => Some(DeclKind::Class),
        1 => Some(DeclKind::Interface),
        2 => Some(DeclKind::Enum),
        3 => Some(DeclKind::Record),
        4 => Some(DeclKind::AnnotationType),
        5 => Some(DeclKind::AnonymousClass),
        6 => Some(DeclKind::Field),
        7 => Some(DeclKind::Method),
        8 => Some(DeclKind::Constructor),
        9 => Some(DeclKind::StaticInitializer),
        10 => Some(DeclKind::InstanceInitializer),
        11 => Some(DeclKind::EnumConstant),
        12 => Some(DeclKind::RecordComponent),
        13 => Some(DeclKind::AnnotationElement),
        14 => Some(DeclKind::Parameter),
        15 => Some(DeclKind::LocalVariable),
        _ => None,
    }
}

/// A stable on-disk encoding for a [`MemberType`]: a one-character tag, then
/// the variant's payload.
///
/// The tag is written out by hand for the same reason [`kind_code`] is —
/// anything derived from declaration order would reinterpret rows a previous
/// version wrote. A text payload is length-prefixed, `C16:java.lang.String`,
/// because the type nests now and text — an unresolved type as written — can
/// hold any delimiter a separator would have to be; and an empty payload is a
/// legitimate value, `U0:` for a member whose declaration writes no type.
fn type_tag(ty: &MemberType) -> String {
    let mut out = String::new();
    encode(ty, &mut out);
    out
}

fn encode(ty: &MemberType, out: &mut String) {
    match ty {
        MemberType::Void => out.push('V'),
        MemberType::Primitive(keyword) => encode_text('P', keyword, out),
        MemberType::Class(fqn) => encode_text('C', fqn, out),
        MemberType::Unresolved(written) => encode_text('U', written, out),
        MemberType::Variable(name) => encode_text('T', name, out),
        MemberType::Parameterized(fqn, arguments) => {
            encode_text('G', fqn, out);
            encode_list(arguments, out);
        }
        MemberType::Extends(bound) => {
            out.push('+');
            encode(bound, out);
        }
        MemberType::Super(bound) => {
            out.push('-');
            encode(bound, out);
        }
    }
}

fn encode_text(tag: char, payload: &str, out: &mut String) {
    out.push(tag);
    out.push_str(&payload.len().to_string());
    out.push(':');
    out.push_str(payload);
}

fn encode_list(types: &[MemberType], out: &mut String) {
    out.push_str(&types.len().to_string());
    out.push(':');
    for ty in types {
        encode(ty, out);
    }
}

/// How deep a stored type may nest before it is refused rather than
/// recursed into: the encoder writes what a reader parsed, which is bounded
/// far below this, so only a corrupt row reaches it.
const MAX_STORED_DEPTH: u32 = 64;

/// The inverse of [`type_tag`]. `None` for a tag this build has no variant
/// for, or a payload that does not parse, so a reader can skip the member it
/// belongs to rather than guess at what it meant.
fn from_type_tag(encoded: &str) -> Option<MemberType> {
    let (ty, rest) = decode(encoded, 0)?;
    rest.is_empty().then_some(ty)
}

fn decode(encoded: &str, depth: u32) -> Option<(MemberType, &str)> {
    if depth > MAX_STORED_DEPTH {
        return None;
    }
    let (tag, rest) = encoded.split_at_checked(1)?;
    match tag {
        "V" => Some((MemberType::Void, rest)),
        "+" | "-" => {
            let (bound, rest) = decode(rest, depth + 1)?;
            let bound = Box::new(bound);
            Some((
                if tag == "+" {
                    MemberType::Extends(bound)
                } else {
                    MemberType::Super(bound)
                },
                rest,
            ))
        }
        "P" | "C" | "U" | "T" | "G" => {
            let (payload, rest) = decode_text(rest)?;
            let payload = payload.to_owned();
            Some(match tag {
                "P" => (MemberType::Primitive(payload), rest),
                "C" => (MemberType::Class(payload), rest),
                "U" => (MemberType::Unresolved(payload), rest),
                "T" => (MemberType::Variable(payload), rest),
                _ => {
                    let (arguments, rest) = decode_list(rest, depth + 1)?;
                    (MemberType::Parameterized(payload, arguments), rest)
                }
            })
        }
        _ => None,
    }
}

/// `<len>:` and that many bytes.
fn decode_count(encoded: &str) -> Option<(usize, &str)> {
    let (count, rest) = encoded.split_once(':')?;
    if count.is_empty() || !count.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((count.parse().ok()?, rest))
}

fn decode_text(encoded: &str) -> Option<(&str, &str)> {
    let (len, rest) = decode_count(encoded)?;
    Some((rest.get(..len)?, rest.get(len..)?))
}

fn decode_list(encoded: &str, depth: u32) -> Option<(Vec<MemberType>, &str)> {
    let (count, mut rest) = decode_count(encoded)?;
    // A count is untrusted: every element is at least one byte, so a count
    // past what is left is corrupt rather than a reason to allocate.
    if count > rest.len() {
        return None;
    }
    let mut types = Vec::with_capacity(count);
    for _ in 0..count {
        let (ty, after) = decode(rest, depth)?;
        types.push(ty);
        rest = after;
    }
    Some((types, rest))
}

/// A formal parameter list, or a supertype's arguments: a count, then each
/// encoded type. `None` when any entry carries a tag this build does not
/// know: a parameter list with a hole in it would let a call match on arity
/// while lying about what it takes, so the member goes rather than the
/// parameter.
fn parameter_tags(parameters: &[MemberType]) -> String {
    let mut out = String::new();
    encode_list(parameters, &mut out);
    out
}

fn from_parameter_tags(encoded: &str) -> Option<Vec<MemberType>> {
    let (types, rest) = decode_list(encoded, 0)?;
    rest.is_empty().then_some(types)
}

/// Type parameter names joined on `,`, which no Java identifier holds.
fn type_parameter_column(names: &[String]) -> String {
    names.join(",")
}

fn from_type_parameter_column(stored: &str) -> Vec<String> {
    if stored.is_empty() {
        return Vec::new();
    }
    stored.split(',').map(str::to_owned).collect()
}

impl CacheDb {
    /// Replace the stored classpath wholesale, in one transaction that also
    /// records the JDK identity a reader must match before trusting the rows.
    ///
    /// Widened from the brief's `write_types(&mut self, facts)`: the "same
    /// transaction" requirement means the JDK's home and version have to be
    /// written alongside the rows, and neither is derivable from `TypeFacts`
    /// itself, so they come in as parameters rather than being set separately
    /// by the caller — a separate call could not share this transaction.
    pub fn write_types(
        &mut self,
        jdk_home: &str,
        jdk_version: &str,
        facts: &[TypeFacts],
    ) -> Result<()> {
        let tx = self.connection.transaction().map_err(to_index_error)?;
        tx.execute_batch("DELETE FROM members; DELETE FROM supertypes; DELETE FROM types;")
            .map_err(to_index_error)?;

        let mut written: i64 = 0;
        {
            let mut insert_type = tx
                .prepare(
                    "INSERT INTO types
                         (fqn, simple, kind, type_params, complete, source, members_complete,
                          name_start, name_end, doc_start, doc_end, is_public)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                )
                .map_err(to_index_error)?;
            let mut insert_super = tx
                .prepare("INSERT INTO supertypes (fqn, super_fqn, arguments) VALUES (?1, ?2, ?3)")
                .map_err(to_index_error)?;
            let mut insert_member = tx
                .prepare(
                    "INSERT INTO members
                         (fqn, position, name, kind, is_static, declared, params, variadic,
                          name_start, name_end, is_abstract, param_names, doc_start, doc_end,
                          own_type_params, is_public)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                             ?16)",
                )
                .map_err(to_index_error)?;

            // Two JDK modules can declare the same class in some builds; an INSERT
            // failure on the primary key would throw away the whole classpath for
            // one collision, so the first writer for an `fqn` wins and the rest
            // are dropped with a log line instead of aborting the write.
            let mut seen = HashSet::with_capacity(facts.len());
            for fact in facts {
                if !seen.insert(fact.fqn.as_str()) {
                    tracing::warn!(fqn = %fact.fqn, "duplicate fqn in classpath, kept the first");
                    continue;
                }
                insert_type
                    .execute((
                        fact.fqn.as_str(),
                        fact.simple.as_str(),
                        kind_code(fact.kind),
                        type_parameter_column(&fact.type_parameters),
                        i64::from(fact.supertypes_complete),
                        fact.source.as_str(),
                        i64::from(fact.members_complete),
                        i64::from(fact.name_start),
                        i64::from(fact.name_end),
                        fact.doc_span.map(|(start, _)| start),
                        fact.doc_span.map(|(_, end)| end),
                        i64::from(fact.is_public),
                    ))
                    .map_err(to_index_error)?;
                for super_fqn in &fact.supertypes {
                    // Empty for a supertype written with no arguments, which
                    // is a different value from a list of none.
                    let arguments = fact
                        .supertype_arguments
                        .iter()
                        .find(|(written, _)| written == super_fqn)
                        .map(|(_, arguments)| parameter_tags(arguments))
                        .unwrap_or_default();
                    insert_super
                        .execute((fact.fqn.as_str(), super_fqn.as_str(), arguments))
                        .map_err(to_index_error)?;
                }
                // Declaration order is the whole key here: two members of a
                // type can share a name, an arity and a kind, and `position`
                // is the only thing that tells them apart on the way back.
                // A constructor is a row of its own kind, split out again on
                // reading.
                for (position, member) in
                    (0i64..).zip(fact.members.iter().chain(&fact.constructors))
                {
                    insert_member
                        .execute((
                            fact.fqn.as_str(),
                            position,
                            member.name.as_str(),
                            kind_code(member.kind),
                            i64::from(member.is_static),
                            type_tag(&member.declared),
                            parameter_tags(&member.parameters),
                            i64::from(member.variadic),
                            i64::from(member.name_start),
                            i64::from(member.name_end),
                            i64::from(member.is_abstract),
                            member.parameter_names.join(","),
                            member.doc_span.map(|(start, _)| start),
                            member.doc_span.map(|(_, end)| end),
                            type_parameter_column(&member.type_parameters),
                            i64::from(member.is_public),
                        ))
                        .map_err(to_index_error)?;
                }
                written += 1;
            }
        }

        for (key, value) in [
            ("jdk.home", jdk_home.to_owned()),
            ("jdk.version", jdk_version.to_owned()),
            ("jdk.facts", written.to_string()),
        ] {
            tx.execute(
                "INSERT INTO metadata (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                (key, value),
            )
            .map_err(to_index_error)?;
        }

        tx.commit().map_err(to_index_error)
    }

    /// Read the stored classpath back. Empty when nothing was ever written.
    ///
    /// Whether these rows still match the JDK on disk is not this function's
    /// call: `jdk.home` and `jdk.version` are readable through [`Self::metadata`],
    /// and the caller compares them before trusting what comes back here.
    ///
    /// A row this build's [`DeclKind`] has no code for is a row a newer
    /// Javelin wrote; it is skipped and
    /// logged rather than failing the whole read, because the rest of the
    /// classpath is still usable. A member whose encoded type carries a tag
    /// this build does not know is skipped the same way and at the same
    /// granularity: the member goes, its type keeps every other member of that
    /// type readable.
    pub fn read_types(&self) -> Result<Vec<TypeFacts>> {
        let mut members = self.read_members()?;

        let mut supertypes: HashMap<String, Vec<String>> = HashMap::new();
        let mut supertype_arguments: HashMap<String, Vec<(String, Vec<MemberType>)>> =
            HashMap::new();
        {
            let mut stmt = self
                .connection
                .prepare("SELECT fqn, super_fqn, arguments FROM supertypes")
                .map_err(to_index_error)?;
            let mut rows = stmt.query([]).map_err(to_index_error)?;
            while let Some(row) = rows.next().map_err(to_index_error)? {
                let fqn: String = row.get(0).map_err(to_index_error)?;
                let super_fqn: String = row.get(1).map_err(to_index_error)?;
                let arguments: String = row.get(2).map_err(to_index_error)?;
                // Unreadable arguments read as raw: the edge survives and
                // its parameters bind to nothing, which claims nothing.
                if !arguments.is_empty()
                    && let Some(arguments) = from_parameter_tags(&arguments)
                {
                    supertype_arguments
                        .entry(fqn.clone())
                        .or_default()
                        .push((super_fqn.clone(), arguments));
                }
                supertypes.entry(fqn).or_default().push(super_fqn);
            }
        }

        let mut stmt = self
            .connection
            .prepare(
                "SELECT fqn, simple, kind, type_params, complete, source, members_complete, \
                 name_start, name_end, doc_start, doc_end, is_public FROM types",
            )
            .map_err(to_index_error)?;
        let mut rows = stmt.query([]).map_err(to_index_error)?;

        let mut facts = Vec::new();
        while let Some(row) = rows.next().map_err(to_index_error)? {
            let fqn: String = row.get(0).map_err(to_index_error)?;
            let simple: String = row.get(1).map_err(to_index_error)?;
            let stored_kind: i64 = row.get(2).map_err(to_index_error)?;
            let type_params: String = row.get(3).map_err(to_index_error)?;
            let complete: i64 = row.get(4).map_err(to_index_error)?;
            let source: String = row.get(5).map_err(to_index_error)?;
            let members_complete: i64 = row.get(6).map_err(to_index_error)?;
            let name_start: u32 = row.get(7).map_err(to_index_error)?;
            let name_end: u32 = row.get(8).map_err(to_index_error)?;
            let doc_start: Option<u32> = row.get(9).map_err(to_index_error)?;
            let doc_end: Option<u32> = row.get(10).map_err(to_index_error)?;
            let is_public: i64 = row.get(11).map_err(to_index_error)?;

            // `debug!`, not `warn!`: reading a row a newer Javelin wrote is an
            // ordinary cross-version read of a cache that is discarded on a
            // schema bump anyway. The duplicate-fqn line above warns because
            // it can change which method a call resolves to.
            let Some(kind) = from_kind_code(stored_kind) else {
                tracing::debug!(fqn = %fqn, stored_kind, "unknown DeclKind code, row skipped");
                continue;
            };

            let (constructors, own) = members
                .remove(&fqn)
                .unwrap_or_default()
                .into_iter()
                .partition(|member| member.kind == DeclKind::Constructor);
            facts.push(TypeFacts {
                constructors,
                doc_span: doc_start.zip(doc_end),
                name_start,
                name_end,
                supertypes: supertypes.remove(&fqn).unwrap_or_default(),
                supertype_arguments: supertype_arguments.remove(&fqn).unwrap_or_default(),
                members: own,
                type_parameters: from_type_parameter_column(&type_params),
                fqn,
                simple,
                kind,
                is_public: is_public != 0,
                supertypes_complete: complete != 0,
                members_complete: members_complete != 0,
                source,
            });
        }
        Ok(facts)
    }

    /// Every stored member, grouped by the `fqn` that declares it and left in
    /// declaration order within each group.
    ///
    /// One ordered pass rather than a query per type: `ORDER BY fqn, position`
    /// is the primary key's own order, so SQLite walks the table once and no
    /// index has to exist for it.
    fn read_members(&self) -> Result<HashMap<String, Vec<MemberFacts>>> {
        let mut stmt = self
            .connection
            .prepare(
                "SELECT fqn, name, kind, is_static, declared, params, variadic,
                        name_start, name_end, is_abstract, param_names, doc_start, doc_end,
                        own_type_params, is_public
                 FROM members ORDER BY fqn, position",
            )
            .map_err(to_index_error)?;
        let mut rows = stmt.query([]).map_err(to_index_error)?;

        let mut members: HashMap<String, Vec<MemberFacts>> = HashMap::new();
        while let Some(row) = rows.next().map_err(to_index_error)? {
            let fqn: String = row.get(0).map_err(to_index_error)?;
            let name: String = row.get(1).map_err(to_index_error)?;
            let stored_kind: i64 = row.get(2).map_err(to_index_error)?;
            let is_static: i64 = row.get(3).map_err(to_index_error)?;
            let declared: String = row.get(4).map_err(to_index_error)?;
            let params: String = row.get(5).map_err(to_index_error)?;
            let variadic: i64 = row.get(6).map_err(to_index_error)?;
            let name_start: i64 = row.get(7).map_err(to_index_error)?;
            let name_end: i64 = row.get(8).map_err(to_index_error)?;
            let is_abstract: i64 = row.get(9).map_err(to_index_error)?;
            let names: String = row.get(10).map_err(to_index_error)?;
            let doc_start: Option<u32> = row.get(11).map_err(to_index_error)?;
            let doc_end: Option<u32> = row.get(12).map_err(to_index_error)?;
            let own: String = row.get(13).map_err(to_index_error)?;
            let is_public: i64 = row.get(14).map_err(to_index_error)?;

            let Some(kind) = from_kind_code(stored_kind) else {
                tracing::debug!(
                    fqn = %fqn, member = %name, stored_kind,
                    "unknown DeclKind code, member skipped"
                );
                continue;
            };
            let Some(declared) = from_type_tag(&declared) else {
                tracing::debug!(
                    fqn = %fqn, member = %name, %declared,
                    "unknown member type tag, member skipped"
                );
                continue;
            };
            let Some(parameters) = from_parameter_tags(&params) else {
                tracing::debug!(
                    fqn = %fqn, member = %name, %params,
                    "unknown member type tag in a parameter, member skipped"
                );
                continue;
            };
            let (Ok(name_start), Ok(name_end)) =
                (u32::try_from(name_start), u32::try_from(name_end))
            else {
                tracing::debug!(
                    fqn = %fqn, member = %name, name_start, name_end,
                    "name span out of range, member skipped"
                );
                continue;
            };

            members.entry(fqn).or_default().push(MemberFacts {
                type_parameters: from_type_parameter_column(&own),
                doc_span: doc_start.zip(doc_end),
                parameter_names: names
                    .split(',')
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .collect(),
                name,
                kind,
                is_static: is_static != 0,
                is_abstract: is_abstract != 0,
                is_public: is_public != 0,
                declared,
                parameters,
                variadic: variadic != 0,
                name_start,
                name_end,
            });
        }
        Ok(members)
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

    /// `source` is derived from `fqn` rather than left empty: a fixed non-empty
    /// value on every call means every test built on this helper exercises the
    /// column surviving a round trip instead of two empty strings comparing
    /// equal by coincidence. The name's span is derived from it the same way.
    fn fact(fqn: &str, kind: DeclKind, supertypes: Vec<&str>, complete: bool) -> TypeFacts {
        TypeFacts {
            is_public: true,
            constructors: Vec::new(),
            doc_span: None,
            name_start: fqn.len() as u32,
            name_end: 2 * fqn.len() as u32,
            fqn: fqn.to_owned(),
            simple: fqn.rsplit('.').next().unwrap_or(fqn).to_owned(),
            kind,
            type_parameters: Vec::new(),
            supertype_arguments: Vec::new(),
            supertypes: supertypes.into_iter().map(str::to_owned).collect(),
            supertypes_complete: complete,
            members_complete: true,
            members: Vec::new(),
            source: format!("{fqn}.java"),
        }
    }

    /// A method with the shapes an encoding has to survive: a resolved return
    /// type, a primitive parameter and an unresolved one that still carries
    /// its type arguments. `name_start`/`name_end` are derived from `name`
    /// rather than left at zero, for the same reason `fact`'s `source` is
    /// derived from `fqn`.
    fn method(name: &str, declared: MemberType, parameters: Vec<MemberType>) -> MemberFacts {
        MemberFacts {
            type_parameters: Vec::new(),
            doc_span: None,
            parameter_names: Vec::new(),
            name: name.to_owned(),
            kind: DeclKind::Method,
            is_static: false,
            is_abstract: false,
            is_public: true,
            declared,
            parameters,
            variadic: false,
            name_start: 100,
            name_end: 100 + name.len() as u32,
        }
    }

    #[test]
    fn member_facts_survive_a_write_and_a_read() {
        let mut db = CacheDb::in_memory().expect("open");
        let mut with_members = fact("java.lang.String", DeclKind::Class, vec![], true);
        with_members.doc_span = Some((10, 40));
        with_members.members = vec![
            method("length", MemberType::Primitive("int".to_owned()), vec![]),
            method(
                "trim",
                MemberType::Class("java.lang.String".to_owned()),
                vec![],
            ),
            method(
                "wait",
                MemberType::Void,
                vec![MemberType::Primitive("long".to_owned())],
            ),
            // The three shapes `MemberType::Unresolved` carries: an
            // unresolved generic keeps its arguments where a resolved one is
            // stripped, and a declaration with no written type at all carries
            // an empty payload — which the encoding must not read as missing.
            method(
                "elsewhere",
                MemberType::Unresolved("Nowhere<String>".to_owned()),
                vec![MemberType::Unresolved(String::new())],
            ),
            MemberFacts {
                type_parameters: Vec::new(),
                doc_span: Some((150, 190)),
                parameter_names: vec!["format".to_owned(), "args".to_owned()],
                name: "format".to_owned(),
                kind: DeclKind::Method,
                is_static: true,
                is_abstract: false,
                is_public: true,
                declared: MemberType::Class("java.lang.String".to_owned()),
                parameters: vec![
                    MemberType::Class("java.lang.String".to_owned()),
                    MemberType::Unresolved("Object[]".to_owned()),
                ],
                variadic: true,
                name_start: 200,
                name_end: 206,
            },
            MemberFacts {
                type_parameters: Vec::new(),
                doc_span: None,
                parameter_names: Vec::new(),
                name: "CASE_INSENSITIVE_ORDER".to_owned(),
                kind: DeclKind::Field,
                is_static: true,
                is_abstract: false,
                is_public: true,
                declared: MemberType::Class("java.util.Comparator".to_owned()),
                parameters: Vec::new(),
                variadic: false,
                name_start: 300,
                name_end: 322,
            },
        ];

        with_members.constructors = vec![MemberFacts {
            kind: DeclKind::Constructor,
            ..method("String", MemberType::Unresolved(String::new()), vec![])
        }];
        db.write_types("/opt/jdk-25", "25", std::slice::from_ref(&with_members))
            .expect("write");

        assert_eq!(db.read_types().expect("read"), vec![with_members]);
    }

    /// Type parameters, a supertype's arguments and every generic member
    /// shape, nested — including text that holds the encoding's own
    /// delimiters, which a length prefix has to carry rather than split on.
    #[test]
    fn generic_facts_survive_a_write_and_a_read() {
        let mut db = CacheDb::in_memory().expect("open");
        let variable = |name: &str| MemberType::Variable(name.to_owned());
        let mut map = fact(
            "java.util.HashMap",
            DeclKind::Class,
            vec![
                "java.util.AbstractMap",
                "java.util.Map",
                "java.io.Serializable",
            ],
            true,
        );
        map.type_parameters = vec!["K".to_owned(), "V".to_owned()];
        map.supertype_arguments = vec![
            (
                "java.util.AbstractMap".to_owned(),
                vec![variable("K"), variable("V")],
            ),
            (
                "java.util.Map".to_owned(),
                vec![variable("K"), variable("V")],
            ),
        ];
        map.members = vec![
            method(
                "get",
                variable("V"),
                vec![MemberType::Class("java.lang.Object".into())],
            ),
            method(
                "entrySet",
                MemberType::Parameterized(
                    "java.util.Set".into(),
                    vec![MemberType::Parameterized(
                        "java.util.Map.Entry".into(),
                        vec![variable("K"), variable("V")],
                    )],
                ),
                vec![],
            ),
            method(
                "forEach",
                MemberType::Void,
                vec![MemberType::Parameterized(
                    "java.util.function.BiConsumer".into(),
                    vec![
                        MemberType::Super(Box::new(variable("K"))),
                        MemberType::Extends(Box::new(MemberType::Unresolved(
                            "Map<K, V>[];:,".into(),
                        ))),
                    ],
                )],
            ),
            method("anything", MemberType::Unresolved("?".into()), vec![]),
        ];
        map.members[0].is_abstract = true;
        map.members[1].is_public = false;
        db.write_types("/opt/jdk", "25", std::slice::from_ref(&map))
            .expect("write");
        let mut read = db.read_types().expect("read");
        for found in &mut read {
            found.supertypes.sort();
            found.supertype_arguments.sort_by(|a, b| a.0.cmp(&b.0));
        }
        map.supertypes.sort();
        map.supertype_arguments.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(read, vec![map]);
    }

    /// `source` and a member's `name_start`/`name_end` are what
    /// `member_definition_at` needs to jump into `src.zip`: the entry to open
    /// and the byte range within it to select. Two types, each with one
    /// member and its own distinct `source` string and asymmetric span, so a
    /// column mixed up with a neighbour's — `source` read from the wrong
    /// type's row, or `name_start`/`name_end` swapped — fails this rather
    /// than passing on a coincidence the other round-trip tests' zeroed-out
    /// fields let through.
    #[test]
    fn a_types_entry_and_its_members_name_spans_survive_a_round_trip() {
        let mut db = CacheDb::in_memory().expect("open");
        let mut first = fact("prov.First", DeclKind::Class, vec![], true);
        first.source = "prov/First.java".to_owned();
        first.members = vec![MemberFacts {
            type_parameters: Vec::new(),
            doc_span: None,
            parameter_names: Vec::new(),
            name: "alpha".to_owned(),
            kind: DeclKind::Method,
            is_static: false,
            is_abstract: false,
            is_public: true,
            declared: MemberType::Void,
            parameters: Vec::new(),
            variadic: false,
            name_start: 42,
            name_end: 47,
        }];
        let mut second = fact("prov.Second", DeclKind::Class, vec![], true);
        second.source = "prov/Second.java".to_owned();
        second.members = vec![MemberFacts {
            type_parameters: Vec::new(),
            doc_span: None,
            parameter_names: Vec::new(),
            name: "beta".to_owned(),
            kind: DeclKind::Method,
            is_static: false,
            is_abstract: false,
            is_public: true,
            declared: MemberType::Void,
            parameters: Vec::new(),
            variadic: false,
            name_start: 900,
            name_end: 904,
        }];

        db.write_types("/opt/jdk", "25", &[first.clone(), second.clone()])
            .expect("write");

        let mut read = db.read_types().expect("read");
        read.sort_by(|a, b| a.fqn.cmp(&b.fqn));
        assert_eq!(read, vec![first, second]);
    }

    #[test]
    fn a_members_declaration_order_survives_a_round_trip() {
        let mut db = CacheDb::in_memory().expect("open");
        // Two overloads that agree on everything the row records except the
        // parameter list, so order is the only thing that can pair each one
        // back up with its own parameters.
        let mut overloaded = fact("over.Loaded", DeclKind::Class, vec![], true);
        overloaded.members = vec![
            method("f", MemberType::Void, vec![MemberType::Void]),
            method(
                "f",
                MemberType::Void,
                vec![MemberType::Primitive("int".to_owned())],
            ),
            method(
                "f",
                MemberType::Void,
                vec![MemberType::Class("java.lang.String".to_owned())],
            ),
        ];

        db.write_types("/opt/jdk", "25", std::slice::from_ref(&overloaded))
            .expect("write");

        let read = db.read_types().expect("read");
        assert_eq!(read, vec![overloaded]);
    }

    #[test]
    fn a_member_with_an_unreadable_type_tag_is_skipped_rather_than_failing_the_read() {
        let mut db = CacheDb::in_memory().expect("open");
        let mut declaring = fact("tag.Tagged", DeclKind::Class, vec![], true);
        let survivor = method("kept", MemberType::Void, vec![]);
        declaring.members = vec![
            method("bad_return", MemberType::Void, vec![]),
            method("bad_parameter", MemberType::Void, vec![MemberType::Void]),
            survivor.clone(),
        ];
        db.write_types("/opt/jdk", "25", &[declaring])
            .expect("write");

        // A tag from a Javelin that knows a fifth `MemberType`, in each of the
        // two columns that hold one.
        db.connection()
            .execute_batch(
                "UPDATE members SET declared = 'Zfuture' WHERE name = 'bad_return';
                 UPDATE members SET params = 'Zfuture' WHERE name = 'bad_parameter';",
            )
            .expect("fake a future tag");

        let read = db
            .read_types()
            .expect("an unreadable member must not fail the read");
        assert_eq!(read.len(), 1, "the type itself is still readable");
        assert_eq!(read[0].members, vec![survivor]);
    }

    #[test]
    fn a_member_with_an_unknown_kind_code_is_skipped_rather_than_failing_the_read() {
        let mut db = CacheDb::in_memory().expect("open");
        let mut declaring = fact("kind.Kinded", DeclKind::Class, vec![], true);
        let survivor = method("kept", MemberType::Void, vec![]);
        declaring.members = vec![
            method("dropped", MemberType::Void, vec![]),
            survivor.clone(),
        ];
        db.write_types("/opt/jdk", "25", &[declaring])
            .expect("write");

        db.connection()
            .execute("UPDATE members SET kind = 999 WHERE name = 'dropped'", [])
            .expect("fake a future DeclKind");

        let read = db
            .read_types()
            .expect("an unknown kind must not fail the read");
        assert_eq!(read.len(), 1, "the type itself is still readable");
        assert_eq!(read[0].members, vec![survivor]);
    }

    #[test]
    fn a_type_with_no_members_reads_back_with_none() {
        let mut db = CacheDb::in_memory().expect("open");
        // Not the same claim as "the members did not survive": a JDK type with
        // no selectable member of its own is ordinary, and it must not be
        // told apart from one whose members were lost.
        let bare = fact("bare.Bare", DeclKind::Class, vec![], true);
        db.write_types("/opt/jdk", "25", std::slice::from_ref(&bare))
            .expect("write");

        let read = db.read_types().expect("read");
        assert_eq!(read, vec![bare]);
        assert!(read[0].members.is_empty());
    }

    #[test]
    fn type_facts_round_trip_through_the_cache() {
        let mut db = CacheDb::in_memory().expect("open");
        let mut facts = vec![
            fact("java.lang.Object", DeclKind::Class, vec![], true),
            fact(
                "java.util.List",
                DeclKind::Interface,
                vec!["java.lang.Iterable", "java.util.Collection"],
                true,
            ),
        ];
        db.write_types("/opt/jdk-25", "25", &facts).expect("write");

        let mut read = db.read_types().expect("read");
        read.sort_by(|a, b| a.fqn.cmp(&b.fqn));
        for f in &mut read {
            f.supertypes.sort();
        }
        facts.sort_by(|a, b| a.fqn.cmp(&b.fqn));
        for f in &mut facts {
            f.supertypes.sort();
        }
        assert_eq!(read, facts);
    }

    #[test]
    fn a_second_write_replaces_the_first_rather_than_appending() {
        let mut db = CacheDb::in_memory().expect("open");
        db.write_types(
            "/opt/jdk-21",
            "21",
            &[fact("a.A", DeclKind::Class, vec![], true)],
        )
        .expect("first write");
        db.write_types(
            "/opt/jdk-25",
            "25",
            &[fact("b.B", DeclKind::Class, vec![], true)],
        )
        .expect("second write");

        let read = db.read_types().expect("read");
        assert_eq!(read, vec![fact("b.B", DeclKind::Class, vec![], true)]);
        assert_eq!(
            db.metadata("jdk.version").expect("read"),
            Some("25".to_owned())
        );
        assert_eq!(
            db.metadata("jdk.home").expect("read"),
            Some("/opt/jdk-25".to_owned())
        );
    }

    #[test]
    fn a_duplicate_fully_qualified_name_does_not_fail_the_write() {
        let mut db = CacheDb::in_memory().expect("open");
        let first = fact("dup.Dup", DeclKind::Class, vec!["dup.First"], true);
        let second = fact("dup.Dup", DeclKind::Interface, vec!["dup.Second"], false);

        db.write_types("/opt/jdk", "21", &[first.clone(), second])
            .expect("a duplicate fqn must not fail the write");

        assert_eq!(db.read_types().expect("read"), vec![first]);
    }

    #[test]
    fn a_cache_from_schema_version_one_is_discarded_not_migrated() {
        let path = scratch("types-schema-v1");
        {
            let mut db = CacheDb::open(&path).expect("create");
            db.write_types(
                "/opt/jdk",
                "21",
                &[fact("a.A", DeclKind::Class, vec![], true)],
            )
            .expect("write");
            db.connection()
                .pragma_update(None, "user_version", 1)
                .expect("fake schema version one");
        }
        let reopened = CacheDb::open(&path).expect("reopening must recover, not fail");
        assert_eq!(reopened.read_types().expect("read"), Vec::new());
        assert_eq!(reopened.metadata("jdk.home").expect("read"), None);
    }

    #[test]
    fn an_incomplete_closure_survives_the_round_trip() {
        let mut db = CacheDb::in_memory().expect("open");
        // One incomplete node with a surviving supertype, one with none at all —
        // `complete` must come back from its own column, never derived from
        // whether `supertypes` happens to be non-empty.
        let with_a_supertype = fact("weird.Weird", DeclKind::Class, vec!["known.Known"], false);
        let with_none = fact("empty.Empty", DeclKind::Class, vec![], false);

        db.write_types(
            "/opt/jdk",
            "21",
            &[with_a_supertype.clone(), with_none.clone()],
        )
        .expect("write");

        let mut read = db.read_types().expect("read");
        read.sort_by(|a, b| a.fqn.cmp(&b.fqn));
        assert_eq!(read, vec![with_none, with_a_supertype]);
    }

    /// Whether a type's members are all of them has a column of its own, and
    /// is never derived from whether its supertypes are.
    #[test]
    fn a_short_member_list_survives_the_round_trip() {
        let mut db = CacheDb::in_memory().expect("open");
        let mut short = fact("short.Short", DeclKind::Class, vec![], true);
        short.members_complete = false;
        let whole = fact("whole.Whole", DeclKind::Class, vec![], false);
        db.write_types("/opt/jdk", "21", &[short.clone(), whole.clone()])
            .expect("write");
        let mut read = db.read_types().expect("read");
        read.sort_by(|a, b| a.fqn.cmp(&b.fqn));
        assert_eq!(read, vec![short, whole]);
    }

    #[test]
    fn writing_forty_thousand_facts_round_trips_completely() {
        let mut db = CacheDb::in_memory().expect("open");
        let facts: Vec<TypeFacts> = (0..40_000)
            .map(|i| {
                fact(
                    &format!("bulk.Type{i}"),
                    DeclKind::Class,
                    vec!["java.lang.Object"],
                    true,
                )
            })
            .collect();

        db.write_types("/opt/jdk", "25", &facts).expect("write");

        let read = db.read_types().expect("read");
        assert_eq!(read.len(), 40_000);
        let sample = read
            .iter()
            .find(|f| f.fqn == "bulk.Type12345")
            .expect("a sampled row is present");
        assert_eq!(sample.simple, "Type12345");
        assert_eq!(sample.supertypes, vec!["java.lang.Object".to_owned()]);
        assert!(sample.supertypes_complete);
        assert_eq!(
            db.metadata("jdk.facts").expect("read"),
            Some("40000".to_owned())
        );
    }
}
