//! Structured logging and latency instrumentation.
//!
//! Two rules hold everywhere in the server:
//!
//! 1. Never write to stdout. Stdout is the LSP transport; a stray `println!`
//!    corrupts the protocol stream. All diagnostics go to stderr or a log file.
//! 2. Prefer structured events over formatted strings, so request latency can be
//!    aggregated instead of grepped.

use std::time::{Duration, Instant};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::BoxMakeWriter;

use crate::config::LoggingConfig;
use crate::error::{Error, Result};

/// Install the global tracing subscriber.
///
/// `JAVELIN_LOG` overrides the configured level, matching `RUST_LOG` syntax.
/// Calling this more than once is a no-op after the first success.
pub fn init(config: &LoggingConfig) -> Result<()> {
    let filter = EnvFilter::try_from_env("JAVELIN_LOG")
        .unwrap_or_else(|_| EnvFilter::new(config.level.as_filter()));

    let writer = match &config.file {
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|source| Error::io(path, source))?;
            BoxMakeWriter::new(file)
        }
        None => BoxMakeWriter::new(std::io::stderr),
    };

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_target(true)
        .with_ansi(false)
        .finish();

    // A second call loses the race harmlessly; tests and embedders both hit this.
    let _ = tracing::subscriber::set_global_default(subscriber);
    Ok(())
}

/// Time an operation and emit it as a structured event.
///
/// Performance is a product requirement, so latency is recorded as data rather
/// than discovered with a stopwatch. Use this at subsystem boundaries, not
/// inside hot loops.
pub fn timed<T>(operation: &'static str, f: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let result = f();
    let elapsed = started.elapsed();
    tracing::debug!(
        operation,
        duration_us = elapsed.as_micros() as u64,
        "operation completed"
    );
    result
}

/// A latency sample, for benchmarks and for the future in-process metrics view.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub operation: &'static str,
    pub elapsed: Duration,
}

impl Timing {
    pub fn measure<T>(operation: &'static str, f: impl FnOnce() -> T) -> (T, Timing) {
        let started = Instant::now();
        let value = f();
        (
            value,
            Timing {
                operation,
                elapsed: started.elapsed(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timed_returns_the_inner_value() {
        assert_eq!(timed("test", || 7), 7);
    }

    #[test]
    fn measure_reports_the_operation_name() {
        let (value, timing) = Timing::measure("parse", || "done");
        assert_eq!(value, "done");
        assert_eq!(timing.operation, "parse");
    }
}
