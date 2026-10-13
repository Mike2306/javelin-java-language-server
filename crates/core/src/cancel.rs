//! Cooperative cancellation.
//!
//! Cancellation is first-class in Javelin: any work whose result the editor no
//! longer needs must stop as soon as it notices. A token is cheap to clone and
//! shares one flag, so a scheduler can hand copies to every worker touching a
//! request and cancel them all at once.
//!
//! Long loops should call [`CancellationToken::check`] at a coarse granularity
//! (per file, per class, per batch) rather than per byte.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Returned by [`CancellationToken::check`] when the work should stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("operation cancelled")]
pub struct Cancelled;

#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    flag: Option<Arc<AtomicBool>>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            flag: Some(Arc::new(AtomicBool::new(false))),
        }
    }

    /// A token that is never cancelled, with no allocation. Use for startup work
    /// and tests, not for anything reachable from an LSP request.
    pub const fn never() -> Self {
        Self { flag: None }
    }

    pub fn cancel(&self) {
        if let Some(flag) = &self.flag {
            flag.store(true, Ordering::Release);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Acquire))
    }

    /// `?`-friendly check to drop into loops.
    pub fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_one_flag() {
        let token = CancellationToken::new();
        let clone = token.clone();
        assert!(clone.check().is_ok());
        token.cancel();
        assert!(clone.is_cancelled());
        assert_eq!(clone.check(), Err(Cancelled));
    }

    #[test]
    fn never_token_stays_live() {
        let token = CancellationToken::never();
        token.cancel();
        assert!(!token.is_cancelled());
    }
}
