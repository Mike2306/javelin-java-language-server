//! The background parse pool.
//!
//! Parsing a file and building its syntax view is the one piece of per-keystroke
//! work whose cost scales with the file rather than with the edit, so it is the
//! one piece that must not run on the request loop: a 300 ms reparse of a
//! generated god-class would otherwise be 300 ms in which the editor gets no
//! answer to anything.
//!
//! Workers see nothing of [`crate::ServerState`]. They receive an immutable
//! document snapshot plus the tree it should be reparsed from, and send back an
//! [`Analysis`] describing exactly the version they were handed. Deciding
//! whether that version is still the one the user is looking at is the loop's
//! job, in [`crate::ServerState::install`].

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, unbounded};
use javelin_core::CancellationToken;
use javelin_document::{DocumentId, EditBatch, TextDocument};
use javelin_parser::{JavaParser, SyntaxTree};

use crate::state::Analysis;

/// Bring one document's analysis up to the version in `document`.
#[derive(Debug)]
pub(crate) struct Job {
    /// Identifies this dispatch, so a result can be matched against what the
    /// loop last asked for rather than against a version number the editor
    /// could reuse after a close and reopen.
    pub seq: u64,
    pub document: Arc<TextDocument>,
    /// The tree to reparse from and the edits that produced this version.
    /// `None` parses the file whole.
    pub previous: Option<(SyntaxTree, EditBatch)>,
    pub token: CancellationToken,
}

impl Job {
    /// Whether the reparse will actually reuse the previous tree.
    ///
    /// The same predicate [`JavaParser::reparse`] applies: it falls back to a
    /// full parse when the edits were computed against another version, which
    /// is routine while the pool is behind the buffer.
    fn reuses_previous_tree(&self) -> bool {
        self.previous
            .as_ref()
            .is_some_and(|(tree, edits)| tree.version() == edits.base_version)
    }
}

/// A finished job, whether or not it produced anything.
#[derive(Debug)]
pub(crate) struct Outcome {
    pub seq: u64,
    pub id: DocumentId,
    /// Whether the tree was reparsed incrementally. Carried only for tests:
    /// nothing on the loop acts on it, and the worker already logs it.
    #[cfg(test)]
    pub incremental: bool,
    /// `None` when the job was cancelled, the parse failed, or the worker
    /// panicked. Every job sends exactly one outcome either way, so the loop's
    /// count of outstanding work stays exact.
    pub analysis: Option<Analysis>,
}

/// A fixed set of threads, each with its own parser.
///
/// One parser per worker rather than one behind a mutex: a `JavaParser` is a
/// grammar handle and some scratch space, and sharing it would serialise the
/// exact work the pool exists to run in parallel.
#[derive(Debug)]
pub(crate) struct Pool {
    /// `Option` so [`Drop`] can close the queue *before* joining. A held sender
    /// keeps every worker blocked on `recv` forever — the same shape as the
    /// shutdown deadlock `run_stdio` guards against.
    jobs: Option<Sender<Job>>,
    results: Receiver<Outcome>,
    /// Kept alive on purpose. If the last worker died, this is what stops the
    /// loop's `select!` from seeing a disconnected channel and spinning.
    _keepalive: Sender<Outcome>,
    workers: Vec<JoinHandle<()>>,
}

impl Pool {
    pub fn new(threads: usize) -> Self {
        let (job_tx, job_rx) = unbounded::<Job>();
        let (result_tx, results) = unbounded::<Outcome>();

        let workers = (0..threads)
            .filter_map(|n| {
                let jobs = job_rx.clone();
                let results = result_tx.clone();
                std::thread::Builder::new()
                    .name(format!("javelin-parse-{n}"))
                    .spawn(move || work(&jobs, &results))
                    .map_err(|err| tracing::error!(%err, "could not spawn a parse worker"))
                    .ok()
            })
            .collect::<Vec<_>>();
        // Not a debug line: with no workers, every `didChange` queues forever
        // and the file's analysis freezes at the version it was opened with,
        // for the rest of the session. `resolved_background_threads` floors the
        // count at 1, so only the OS refusing every thread gets here.
        if workers.is_empty() {
            tracing::error!("no parse workers started; documents will not be reparsed");
        } else {
            tracing::debug!(workers = workers.len(), "parse pool started");
        }

        Self {
            jobs: Some(job_tx),
            results,
            _keepalive: result_tx,
            workers,
        }
    }

    /// Queue a job. Never blocks, which is the point: the loop must not wait on
    /// a worker even for the length of a channel send.
    ///
    /// ponytail: the queue is unbounded, and a superseded job is only dropped
    /// when a worker picks it up and sees its cancelled token — so edits
    /// arriving faster than the pool drains pin one document snapshot each. A
    /// human typing cannot outrun it (a `Tier::Large` reparse plus both view
    /// builds is ~35 ms against ~100 ms between keystrokes) but a client
    /// replaying a file can, and each queued job holds the whole buffer.
    /// The upgrade is one slot per document instead of a queue, so a new edit
    /// overwrites the pending job rather than queueing behind it; do it if a
    /// memory profile under a burst says so.
    pub fn submit(&self, job: Job) {
        if let Some(jobs) = &self.jobs {
            // Unreachable while the pool is alive; a closed queue only happens
            // during drop, when the answer is no longer wanted anyway.
            let _ = jobs.send(job);
        }
    }

    pub fn results(&self) -> &Receiver<Outcome> {
        &self.results
    }

    /// How many workers are actually running.
    #[cfg(test)]
    pub fn size(&self) -> usize {
        self.workers.len()
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        drop(self.jobs.take());
        for worker in self.workers.drain(..) {
            // A panicked worker is a bug worth a loud line, but not a reason to
            // panic the thread that is trying to shut the session down.
            if worker.join().is_err() {
                tracing::error!("a parse worker panicked");
            }
        }
    }
}

fn work(jobs: &Receiver<Job>, results: &Sender<Outcome>) {
    let mut parser = match JavaParser::new() {
        Ok(parser) => parser,
        // `ServerState::new` loads the same grammar on the loop thread and fails
        // startup if it cannot, so this is close to unreachable.
        Err(err) => {
            tracing::error!(%err, "a parse worker could not load the Java grammar");
            return;
        }
    };

    for job in jobs {
        let (seq, id) = (job.seq, job.document.id().clone());
        let incremental = job.reuses_previous_tree();
        // A panic here would otherwise take the worker with it and stop this
        // document being served for the rest of the session. Unwinding leaves
        // the parser's own state untouched: only the borrow crosses the
        // boundary, and a failed job is answered like a cancelled one.
        let analysis =
            std::panic::catch_unwind(AssertUnwindSafe(|| run(&mut parser, job, incremental)))
                .unwrap_or_else(|_| {
                    tracing::error!(uri = id.as_str(), "a parse panicked; dropping the result");
                    None
                });
        let outcome = Outcome {
            seq,
            id,
            #[cfg(test)]
            incremental,
            analysis,
        };
        if results.send(outcome).is_err() {
            return;
        }
    }
}

fn run(parser: &mut JavaParser, job: Job, incremental: bool) -> Option<Analysis> {
    // ponytail: cancellation is checked between phases, not inside them:
    // `JavaParser::parse`/`reparse` take no token, and neither view builder
    // does. The ceiling is that shutdown can wait for one parse of one file;
    // if that ever shows up, stop the parse through tree-sitter's safe
    // `ParseOptions::progress_callback` and push the token into
    // `FileSyntax::build`/`ScopeTree::build`.
    job.token.check().ok()?;

    let started = std::time::Instant::now();
    let parsed = match &job.previous {
        Some((tree, edits)) => parser.reparse(tree, edits, &job.document),
        None => parser.parse(&job.document),
    };
    let tree = match parsed {
        Ok(tree) => tree,
        // Never fatal: an unparseable buffer costs this file its syntax
        // features for this version, not the session. The previously installed
        // analysis stays, and it still describes a version this file really had.
        Err(err) => {
            tracing::warn!(uri = job.document.id().as_str(), %err, "parse failed");
            return None;
        }
    };
    job.token.check().ok()?;

    let mut analysis = Analysis::new(job.document, tree);
    analysis.warm();
    tracing::debug!(
        uri = analysis.document().id().as_str(),
        version = analysis.version(),
        incremental,
        duration_us = started.elapsed().as_micros() as u64,
        "reparsed document"
    );
    Some(analysis)
}

#[cfg(test)]
mod tests {
    use super::*;
    use javelin_document::TextDocument;

    fn job(seq: u64, token: CancellationToken) -> Job {
        Job {
            seq,
            document: Arc::new(TextDocument::new(
                DocumentId::new("file:///A.java"),
                1,
                "class A { int x; }",
            )),
            previous: None,
            token,
        }
    }

    #[test]
    fn a_submitted_job_comes_back_as_an_analysis_of_the_same_version() {
        let pool = Pool::new(1);
        pool.submit(job(7, CancellationToken::never()));
        let outcome = pool.results().recv().expect("an outcome");
        assert_eq!(outcome.seq, 7);
        let analysis = outcome.analysis.expect("a parse");
        assert_eq!(analysis.version(), 1);
        // Warmed on the worker, so the request loop pays for neither.
        assert!(analysis.is_warm());
    }

    #[test]
    fn a_cancelled_job_still_answers_so_nothing_waits_for_it_forever() {
        let pool = Pool::new(1);
        let token = CancellationToken::new();
        token.cancel();
        pool.submit(job(1, token));
        let outcome = pool.results().recv().expect("an outcome");
        assert_eq!(outcome.seq, 1);
        assert!(outcome.analysis.is_none());
    }

    /// The queue is closed before the join, so this returns rather than hanging.
    #[test]
    fn dropping_the_pool_joins_every_worker() {
        let pool = Pool::new(3);
        assert_eq!(pool.size(), 3);
        drop(pool);
    }
}
