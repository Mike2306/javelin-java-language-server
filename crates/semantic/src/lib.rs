//! Java semantic analysis: types, scopes and symbol resolution.
//!
//! Resolution reaches as far as one file — the file the query is currently in,
//! which is the buffer the request named, or a file a walk followed a
//! declaration into — and then as far as [`Externals`] can see, the one window
//! this crate has onto a classpath it deliberately does not depend on. What a
//! *written* name means is settled by one file and never by the set: which
//! file a name refers to is a package-and-import question, and answering it
//! needs an index this crate does not have. What neither settles is [`Type::Unknown`],
//! and that is an ordinary answer rather than an error. [`NoExternals`] closes
//! the window entirely, which is the open-buffer-only behaviour this crate had
//! before a classpath existed.
//!
//! This crate does not depend on tree-sitter and never names a grammar node
//! kind. Where it needs an expression, `javelin-parser` projects one into
//! [`javelin_parser::Expr`] first.

mod apply;
mod externals;
mod files;
mod imports;
mod infer;
mod members;
mod missing;
mod queries;
mod resolve;
mod types;

pub use externals::{
    ExternalMember, ExternalMembers, ExternalSite, ExternalType, Externals, NoExternals,
};
pub use files::{DeclSite, FileId, Files};
pub use infer::{
    HintedType, TypeDefinition, TypeTarget, hinted_type, lambda_parameter_hint, type_definition_at,
    type_of,
};
pub use members::{Member, exposes_member, member_type, members_of};
pub use missing::{MemberUse, MissingMember, missing_member_at};
pub use queries::{
    ConstructorTarget, ExternalSummary, ExternalTypeRef, MemberCompletions, SelectedConstructor,
    call_definition_at, call_parameter_names_at, constructor_at, external_member_at,
    external_type_at, member_completions_at, member_definition_at, scope_answers_case_label,
};
pub use resolve::resolve_written;
pub use types::{Primitive, Type};
