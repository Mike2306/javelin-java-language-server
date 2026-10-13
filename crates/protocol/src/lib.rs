//! The contract between the LSP layer and the engine.
//!
//! This crate exists so `javelin-server` can be the only thing that knows about
//! LSP, and the engine crates can be the only things that know about Java.
//! Nothing here depends on `lsp-types` or on any editor.
//!
//! Its one concept today is *progressive availability*: what the engine can
//! answer right now. Javelin's central promise is that the editor becomes useful
//! immediately and gets smarter in the background, which means "not ready yet"
//! is a normal, first-class answer rather than an error.

use serde::{Deserialize, Serialize};

/// How much of the engine has warmed up.
///
/// Levels are ordered, and each one strictly includes the ones below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Readiness {
    /// The process is alive; documents can be opened.
    #[default]
    Starting,
    /// The open file parses. Outline and syntax-level answers work.
    Syntax,
    /// Declarations within the open file resolve. Local navigation and
    /// completion work.
    File,
    /// Workspace sources are indexed. Cross-file navigation works.
    Workspace,
    /// Dependencies and the JDK are indexed. Everything is available.
    Dependencies,
}

/// A capability a client can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Feature {
    DocumentSymbols,
    Completion,
    Hover,
    Definition,
    References,
    WorkspaceSymbols,
    Diagnostics,
    CallHierarchy,
    Rename,
}

impl Feature {
    /// The readiness level at which this feature starts returning useful, if
    /// incomplete, results.
    ///
    /// The point of the mapping is that a request must never *wait* for a higher
    /// level. It answers with what the current level supports and the client sees
    /// better answers as the engine warms up.
    pub fn available_from(self) -> Readiness {
        match self {
            Feature::DocumentSymbols => Readiness::Syntax,
            Feature::Completion | Feature::Hover | Feature::Definition => Readiness::File,
            Feature::References | Feature::WorkspaceSymbols | Feature::Diagnostics => {
                Readiness::Workspace
            }
            Feature::CallHierarchy | Feature::Rename => Readiness::Dependencies,
        }
    }
}

/// What the engine currently supports, published by the scheduler and read by
/// request handlers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineState {
    pub readiness: Readiness,
}

impl EngineState {
    pub fn supports(&self, feature: Feature) -> bool {
        self.readiness >= feature.available_from()
    }
}

/// The result of a request that may only be partially answerable.
///
/// Handlers return `Partial` instead of blocking or erroring, and the server
/// turns it into a normal response plus a hint that a better answer will follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer<T> {
    /// Everything the feature can ever offer for this input.
    Complete(T),
    /// The best answer at the current readiness level.
    Partial { value: T, complete_at: Readiness },
    /// Nothing useful yet; the client should retry once readiness improves.
    NotYet { complete_at: Readiness },
}

impl<T> Answer<T> {
    pub fn value(self) -> Option<T> {
        match self {
            Answer::Complete(value) | Answer::Partial { value, .. } => Some(value),
            Answer::NotYet { .. } => None,
        }
    }

    pub fn is_complete(&self) -> bool {
        matches!(self, Answer::Complete(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_freshly_started_engine_can_already_outline_a_file() {
        let state = EngineState {
            readiness: Readiness::Syntax,
        };
        assert!(state.supports(Feature::DocumentSymbols));
        assert!(!state.supports(Feature::Completion));
    }

    #[test]
    fn nothing_needs_dependencies_before_local_navigation_works() {
        assert!(Feature::Definition.available_from() < Readiness::Workspace);
        assert!(Feature::Completion.available_from() < Readiness::Workspace);
    }

    #[test]
    fn readiness_levels_are_cumulative() {
        let state = EngineState {
            readiness: Readiness::Dependencies,
        };
        for feature in [
            Feature::DocumentSymbols,
            Feature::Completion,
            Feature::Hover,
            Feature::Definition,
            Feature::References,
            Feature::WorkspaceSymbols,
            Feature::Diagnostics,
            Feature::CallHierarchy,
            Feature::Rename,
        ] {
            assert!(state.supports(feature), "{feature:?}");
        }
    }

    #[test]
    fn a_partial_answer_still_carries_a_value() {
        let answer = Answer::Partial {
            value: vec![1, 2],
            complete_at: Readiness::Workspace,
        };
        assert!(!answer.is_complete());
        assert_eq!(answer.value(), Some(vec![1, 2]));
    }
}
