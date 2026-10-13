//! Tracking of in-flight requests so they can be abandoned.
//!
//! Editors cancel constantly: every keystroke invalidates the completion request
//! from the previous one. Work that nobody is waiting for still costs CPU that
//! the next request needs, so a cancelled request must actually stop rather than
//! run to completion and have its answer thrown away.

use std::collections::HashMap;

use javelin_core::CancellationToken;
use lsp_server::RequestId;

/// The cancellation tokens of every request currently being served.
#[derive(Debug, Default)]
pub struct InFlight {
    tokens: HashMap<RequestId, CancellationToken>,
}

impl InFlight {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a request and hand back the token its handler must poll.
    pub fn begin(&mut self, id: RequestId) -> CancellationToken {
        let token = CancellationToken::new();
        self.tokens.insert(id, token.clone());
        token
    }

    pub fn finish(&mut self, id: &RequestId) {
        self.tokens.remove(id);
    }

    /// Cancel a request. Unknown ids are ignored: `$/cancelRequest` routinely
    /// races a response that has already been sent.
    pub fn cancel(&mut self, id: &RequestId) -> bool {
        match self.tokens.remove(id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// Cancel everything, used on shutdown.
    pub fn cancel_all(&mut self) {
        for (_, token) in self.tokens.drain() {
            token.cancel();
        }
    }

    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelling_a_request_trips_the_handlers_token() {
        let mut in_flight = InFlight::new();
        let id = RequestId::from(1);
        let token = in_flight.begin(id.clone());
        assert!(token.check().is_ok());
        assert!(in_flight.cancel(&id));
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancelling_an_already_answered_request_is_harmless() {
        let mut in_flight = InFlight::new();
        let id = RequestId::from(7);
        in_flight.begin(id.clone());
        in_flight.finish(&id);
        assert!(!in_flight.cancel(&id));
    }

    #[test]
    fn shutdown_stops_everything_still_running() {
        let mut in_flight = InFlight::new();
        let tokens: Vec<_> = (0..3)
            .map(|i| in_flight.begin(RequestId::from(i)))
            .collect();
        in_flight.cancel_all();
        assert!(in_flight.is_empty());
        assert!(tokens.iter().all(|t| t.is_cancelled()));
    }
}
