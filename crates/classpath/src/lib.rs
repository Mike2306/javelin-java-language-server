//! The model of Java code that is not workspace source: the JDK, JARs, and
//! compiled classes.
//!
//! This crate merges two closely coupled concerns that would otherwise be empty
//! crates on their own:
//!
//! - `jdk` — discovering installed JDKs and reading their release metadata.
//!   Modelling the platform modules a project compiles against is later work;
//!   see `docs/roadmap.md`, phase 3.
//! - `classfile` — reading a `.class` file's declarations into `TypeFacts`:
//!   names, supertypes and members, member types from `Signature` so nothing
//!   erased is claimed. No `Code`, no verification (ADR 0005).
//! - `resolve` — turning a type name as written into a fully-qualified one
//!   against a table of known type names, the shared rule the src.zip reader
//!   and the server both resolve names through.
//! - `sources` — reading `src.zip` into `TypeFacts`: every type the JDK's own
//!   sources declare, with its supertype closure and its members' types
//!   resolved through `resolve`, each carrying the zip entry it was read from
//!   and each member the byte span of its own name within that entry.
//!   `SourceArchive` reads one entry's text back out on demand, for whatever
//!   later turns a span into a jump.
//! - `lombok` — the members Lombok's annotations generate on a source type,
//!   read off the same `Decl` `written_members` reads, for the workspace walk.
//! - `sources_jar` — a dependency's `-sources.jar` beside it, read for where
//!   each name a class file declares is written, so a jump has a place.
//! - `world` — `TypeWorld`, the immutable in-memory index over a `Vec<TypeFacts>`
//!   the request path asks against: does a name exist, can one type be a
//!   subtype of another, and — a fact at a time, for the server's member
//!   closure — what does a type declare.
//!
//! The performance constraint that shapes this crate: reading the JDK is the
//! single largest one-off cost in a Java language server. It must happen in the
//! background, be cached across restarts by `javelin-index`, and never sit on the
//! path between the editor starting and the first file becoming useful.

mod classfile;
mod jdk;
mod lombok;
mod resolve;
mod sources;
mod sources_jar;
mod world;

pub use classfile::{read_class, read_jar_facts, read_jar_facts_reporting, registers_processor};
pub use jdk::{Jdk, discover};
pub use lombok::{Generated, GeneratedType, LombokUse, lombok_members};
pub use resolve::{Resolution, TypeTable, simple_name};
pub use sources::{
    MemberFacts, MemberType, SourceArchive, TypeFacts, WrittenMember, implicit_members,
    names_a_type_parameter, read_type_facts, resolve_generic_with, type_parameter_names,
    written_constructors, written_members, written_type_name,
};
pub use sources_jar::{IN_ARCHIVE, graft_sources, sources_jar_for};
pub use world::TypeWorld;
// `TypeFacts::kind` is already public; re-exporting its type is what lets a crate
// that depends on `javelin-classpath` alone (`javelin-index`, storing the facts)
// name and match on it without a direct dependency on `javelin-parser` too.
pub use javelin_parser::DeclKind;
