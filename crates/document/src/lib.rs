//! The open-document store: what the editor currently believes each file says.
//!
//! Everything downstream reads Java source through this crate, never from disk
//! directly, because the buffer in the editor and the bytes on disk diverge the
//! moment the user types. Documents are handed out as immutable [`Arc`]
//! snapshots so a background task can keep analysing revision 7 while the user
//! is already producing revision 9.
//!
//! [`Arc`]: std::sync::Arc

mod line_index;
mod store;
mod text;

pub use line_index::{BytePosition, LineIndex, Position};
pub use store::DocumentStore;
pub use text::{Change, DocumentId, EditBatch, Range, TextDocument, TextEdit};
