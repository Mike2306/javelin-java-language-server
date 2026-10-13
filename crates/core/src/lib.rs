//! Shared foundation for the Javelin Java language server.
//!
//! Everything in this crate is deliberately editor-agnostic and Java-agnostic:
//! configuration, the error taxonomy, cancellation, and tracing setup. Higher
//! layers depend on `javelin-core`; `javelin-core` depends on nobody.

pub mod cancel;
pub mod config;
pub mod error;
pub mod paths;
pub mod telemetry;

pub use cancel::{CancellationToken, Cancelled};
pub use config::Config;
pub use error::{Error, Result};
