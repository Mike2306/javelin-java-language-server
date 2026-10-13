//! The error taxonomy for the whole server.
//!
//! The split exists so callers can decide *how loud* a failure should be. A
//! malformed Java file or a stale `pom.xml` must degrade a single feature, never
//! take the process down. Only [`Error::Internal`] indicates a programming bug.

use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// How a failure should be surfaced to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Degrade quietly: log at debug level, keep serving. Stale or partial data
    /// is better than an error popup while the user is typing.
    Degrade,
    /// Worth telling the user once, e.g. a broken configuration file.
    Report,
    /// A bug in Javelin. Log at error level with full context.
    Bug,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The user's configuration is malformed or contradictory.
    #[error("invalid configuration: {message}")]
    Config { message: String },

    /// Filesystem access failed.
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A source file could not be parsed into anything usable. Expected during
    /// normal editing and never fatal.
    #[error("failed to parse {path}: {message}")]
    Parse { path: PathBuf, message: String },

    /// A build system (Maven, Gradle) could not be queried or produced garbage.
    #[error("build system error ({system}): {message}")]
    Build { system: String, message: String },

    /// The persistent index or cache is unusable. Recovery is to rebuild it.
    #[error("index error: {message}")]
    Index { message: String },

    /// The work was cancelled because its result is no longer needed.
    #[error("operation cancelled")]
    Cancelled,

    /// An invariant inside Javelin was violated.
    #[error("internal error: {message}")]
    Internal { message: String },
}

impl Error {
    pub fn severity(&self) -> Severity {
        match self {
            Error::Parse { .. } | Error::Cancelled | Error::Build { .. } => Severity::Degrade,
            Error::Config { .. } | Error::Io { .. } | Error::Index { .. } => Severity::Report,
            Error::Internal { .. } => Severity::Bug,
        }
    }

    /// True when the caller should keep the server running and simply return a
    /// less complete answer.
    pub fn is_recoverable(&self) -> bool {
        self.severity() != Severity::Bug
    }

    pub fn config(message: impl Into<String>) -> Self {
        Error::Config {
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Error::Internal {
            message: message.into(),
        }
    }

    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}

impl From<crate::cancel::Cancelled> for Error {
    fn from(_: crate::cancel::Cancelled) -> Self {
        Error::Cancelled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_broken_java_file_never_looks_like_a_bug() {
        let err = Error::Parse {
            path: "A.java".into(),
            message: "unexpected token".into(),
        };
        assert!(err.is_recoverable());
        assert_eq!(err.severity(), Severity::Degrade);
    }

    #[test]
    fn internal_errors_are_flagged_as_bugs() {
        assert_eq!(Error::internal("bad state").severity(), Severity::Bug);
        assert!(!Error::internal("bad state").is_recoverable());
    }
}
