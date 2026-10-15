//! The workspace's own sources: the background job that indexes them, and
//! where the roots to index come from.
//!
//! Shaped like [`crate::classpath`] and for the same reasons: a detached
//! thread started from the `initialized` notification, a cancellation token
//! nothing ever waits on, and a finished immutable index arriving on a channel
//! the request loop already selects over. A hundred thousand files is another
//! thread's afternoon and none of the editor's.
//!
//! Unlike the classpath, this store is per workspace — the JDK's facts warm
//! every project on the machine, a project's own sources warm exactly one —
//! so it lives under `javelin_core::paths::workspace_cache_dir` and is keyed
//! by the first root.
//!
//! Two things read the published index. `workspace/symbol` lists the names in
//! it, and `Externals::resolve` — through [`crate::classpath::ClasspathExternals`]
//! — makes them *mean* something: a name the open buffer does not declare can
//! now resolve to a sibling source file, which is what definition and hover
//! follow into it. References is the reader still owed.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use javelin_classpath::TypeWorld;
use javelin_core::{CancellationToken, Cancelled, Config};
use javelin_index::{
    CacheDb, REFS_WITH_JDK, REFS_WITHOUT_WORLD, SourceChange, WorkspaceIndex, refs_settled,
};
use javelin_parser::JavaParser;

/// A file the loop says has changed, on its way to the worker.
///
/// A buffer travels as the document itself, a refcount on the loop, and is
/// only copied into text on the worker's thread.
#[derive(Debug, Clone)]
pub(crate) enum Job {
    /// Re-read from disk: a save, a close, a watcher's report.
    Disk(PathBuf),
    /// Index an open buffer's current text.
    Buffer(PathBuf, Arc<javelin_document::TextDocument>),
    /// Walk the whole workspace again: a folder was created or deleted, and
    /// no single path says which files that took with it.
    Rescan,
    /// A classpath has arrived: member references resolved against a lesser
    /// world are resolved again. The key is the settled world's — the JDK's
    /// and the dependencies' as the build tool last answered — and `None`
    /// while the dependencies may still be coming.
    World(Arc<TypeWorld>, Option<u64>),
}

/// Jobs folded as they arrive: the last one per path, and a rescan that
/// makes every disk read before it redundant.
///
/// Folded on arrival rather than collected and folded later, so a burst's
/// memory is one job per file rather than one per keystroke, and the fold is
/// linear in the burst.
#[derive(Debug, Default)]
pub(crate) struct Batch {
    world: Option<(Arc<TypeWorld>, Option<u64>)>,
    rescan: bool,
    order: Vec<PathBuf>,
    jobs: std::collections::HashMap<PathBuf, Job>,
}

impl Batch {
    pub(crate) fn push(&mut self, job: Job) {
        match job {
            Job::World(world, settled) => self.world = Some((world, settled)),
            Job::Rescan => {
                // A rescan reads the disk for every file, so a disk read
                // queued before it is subsumed; a buffer is not, because the
                // buffer and not the disk is what the user is reading.
                self.rescan = true;
                self.jobs.retain(|_, job| matches!(job, Job::Buffer(..)));
                self.order.retain(|path| self.jobs.contains_key(path));
            }
            Job::Disk(ref path) | Job::Buffer(ref path, _) => {
                let path = path.clone();
                if self.jobs.insert(path.clone(), job).is_none() {
                    self.order.push(path);
                }
            }
        }
    }

    /// The folded jobs: a rescan first if there was one, then one job per
    /// path in the order each path first arrived.
    pub(crate) fn into_jobs(mut self) -> Vec<Job> {
        let mut out = Vec::with_capacity(self.order.len() + 2);
        if let Some((world, settled)) = self.world {
            out.push(Job::World(world, settled));
        }
        if self.rescan {
            out.push(Job::Rescan);
        }
        for path in self.order {
            if let Some(job) = self.jobs.remove(&path) {
                out.push(job);
            }
        }
        out
    }
}

/// [`Batch`]'s fold over a list, for a test that has one.
#[cfg(test)]
pub(crate) fn coalesce(jobs: Vec<Job>) -> Vec<Job> {
    let mut batch = Batch::default();
    for job in jobs {
        batch.push(job);
    }
    batch.into_jobs()
}

/// `first` and whatever follows it until `quiet` passes with nothing new, or
/// `longest` passes since `first` — so a typist who never pauses still sees
/// the index move twice a second rather than never. Beside the folded batch,
/// how many jobs it folded.
pub(crate) fn gather(
    first: Job,
    jobs: &Receiver<Job>,
    quiet: Duration,
    longest: Duration,
) -> (Vec<Job>, usize) {
    let started = Instant::now();
    let mut batch = Batch::default();
    batch.push(first);
    let mut received = 1;
    loop {
        let left = longest.saturating_sub(started.elapsed());
        if left.is_zero() {
            break;
        }
        match jobs.recv_timeout(quiet.min(left)) {
            Ok(job) => {
                received += 1;
                batch.push(job);
            }
            Err(_) => break,
        }
    }
    (batch.into_jobs(), received)
}

/// A finished walk, on its way to the loop.
#[derive(Debug)]
pub(crate) struct Workspace {
    pub index: Arc<WorkspaceIndex>,
    pub elapsed: Duration,
    /// `false` for the walk at `initialized`, `true` for every re-index after
    /// it — which is only what the log line says.
    pub refreshed: bool,
    /// What the last walk saw besides the files it took; `None` from a
    /// caller that did not walk — a test handing over an index.
    pub survey: Option<Arc<javelin_index::Survey>>,
    /// How many of the loop's jobs this index has taken in since the last
    /// publish, so the loop knows when the index is level with every buffer.
    pub absorbed: usize,
}

/// Where this workspace's source index is stored.
///
/// One function so the job and `doctor` cannot disagree about which file they
/// mean — a mismatch would have `doctor` report an empty index beside a server
/// that has one.
pub fn cache_path(config: &Config, workspace: &Path) -> javelin_core::Result<PathBuf> {
    let root = javelin_core::paths::cache_root(config.index.cache_dir.as_deref())?;
    Ok(javelin_core::paths::workspace_cache_dir(&root, workspace).join("sources.sqlite"))
}

/// Start the walk on a thread of its own, if there is anything to walk.
///
/// Answers whether a thread was started. Never joined, for the reason
/// [`crate::classpath::spawn`] documents: a walk can be minutes from finishing
/// when the user closes the editor, and a shutdown that waited for it is the
/// "server did not shut down" dialog.
///
/// `world_coming` says a classpath thread is running, so the first resolution
/// pass is worth holding back until its settled world arrives.
pub(crate) fn spawn(
    roots: Vec<PathBuf>,
    config: &Config,
    cancel: CancellationToken,
    results: Sender<Workspace>,
    jobs: Receiver<Job>,
    world_coming: bool,
    progress: crate::progress::Reporter,
) -> bool {
    if !config.index.background_indexing {
        tracing::info!("workspace: background indexing is off, nothing will be indexed");
        return false;
    }
    if roots.is_empty() {
        tracing::info!("workspace: the client named no folder, so there is nothing to index");
        return false;
    }
    let config = config.clone();
    match std::thread::Builder::new()
        .name("javelin-workspace".to_owned())
        .spawn(move || {
            run(
                &roots,
                &config,
                &cancel,
                &results,
                &jobs,
                world_coming,
                progress,
            );
        }) {
        Ok(_) => true,
        Err(err) => {
            tracing::warn!(%err, "workspace: could not start the indexing thread");
            false
        }
    }
}

/// How long the worker waits for the next change before re-indexing what has
/// queued: one re-index per pause in typing, one per `git checkout`.
const QUIET: Duration = Duration::from_millis(250);

/// Add to `touched` what a file's change from declaring `before` to `after`
/// could move: every fully-qualified name either side declares, and the
/// simple name of each one only one side declares.
///
/// A type on both sides can have changed its members, which its
/// fully-qualified name catches; it cannot have changed what a spelling
/// resolves to. Matching its simple name too would re-resolve every file that
/// writes `Builder` whenever a file of generated builders is edited.
fn touch(touched: &mut BTreeSet<String>, before: Vec<String>, after: Vec<String>) {
    let before: BTreeSet<String> = before.into_iter().collect();
    let after: BTreeSet<String> = after.into_iter().collect();
    for fqn in before.symmetric_difference(&after) {
        if let Some(simple) = fqn.rsplit('.').next() {
            touched.insert(simple.to_owned());
        }
    }
    touched.extend(before);
    touched.extend(after);
}

/// Files whose member references are stored per transaction: few enough that
/// a killed session loses little, many enough not to fsync per file.
const STORED_PER_BATCH: usize = 256;

/// The longest a change waits to be published while changes keep arriving.
const LONGEST: Duration = Duration::from_millis(500);

/// How long after it starts the worker holds its first resolution pass back
/// for the settled classpath. A warm one is in before the walk is done; a
/// build tool that takes longer costs a second pass when it does answer,
/// which is cheaper than no cross-file references for minutes.
const SETTLE_WAIT: Duration = Duration::from_secs(30);

/// How long it waits for the JDK at all: an eleven-second `src.zip` read on a
/// slow disk is still cheaper to wait for than a pass against nothing.
const JDK_WAIT: Duration = Duration::from_secs(120);

fn run(
    roots: &[PathBuf],
    config: &Config,
    cancel: &CancellationToken,
    results: &Sender<Workspace>,
    jobs: &Receiver<Job>,
    world_coming: bool,
    progress: crate::progress::Reporter,
) {
    let started = Instant::now();
    let walking = progress.begin("Indexing the workspace");
    let (found, survey) =
        javelin_index::java_sources_surveyed(roots, config.index.max_file_size_bytes, cancel);
    tracing::info!(files = found.len(), "workspace: indexing");

    // `persist` is the user saying "do not reuse an index from a previous
    // session". An in-memory database honours that without a second code
    // path: the same walk happens, everything is parsed, and nothing is left
    // behind for a later session to find.
    let db = if config.index.persist {
        open(config, &roots[0])
    } else {
        CacheDb::in_memory()
            .inspect_err(|err| tracing::warn!(%err, "workspace: no in-memory index"))
            .ok()
    };
    let Some(mut db) = db else {
        return;
    };
    db.set_lombok_renamed(survey.lombok_renamed.iter().cloned());
    db.set_lombok_log(survey.lombok_log.clone());
    let mut worker = Worker {
        roots,
        config,
        cancel,
        results,
        db,
        world: Arc::new(TypeWorld::empty()),
        settled: None,
        current: Arc::new(WorkspaceIndex::default()),
        survey: Arc::new(survey),
        absorbed: 0,
        progress,
        resolving: None,
    };

    // What each file declared before this walk, for the files it turns out to
    // have changed: a type that disappeared is one whose dependents moved.
    let before: HashMap<PathBuf, Vec<String>> = worker
        .db
        .read_source_types()
        .map(|types| {
            let mut by_file: HashMap<PathBuf, Vec<String>> = HashMap::new();
            for ty in types {
                by_file.entry(ty.file).or_default().push(ty.fqn);
            }
            by_file
        })
        .unwrap_or_default();
    let walked = match worker.db.index_sources_reporting(&found, cancel) {
        Ok(walked) => walked,
        Err(err) => {
            tracing::warn!(%err, "workspace: indexing failed");
            return;
        }
    };
    worker.current = Arc::new(walked.index);
    // Types, occurrences and members are useful at once; member references
    // follow in a second publish once every changed file is resolved.
    worker.send(started, false);
    drop(walking);
    if config.index.persist {
        prune(config, &roots[0]);
    }
    let mut changed: Vec<(PathBuf, Option<String>)> = walked
        .changed
        .into_iter()
        .map(|path| (path, None))
        .collect();
    // Resolving against the JDK alone and again when the JARs arrive is two
    // whole passes where one would do, and the first answers nothing through
    // a dependency type; a warm classpath is in before the walk is done.
    //
    // Only when there is something to resolve: while it waits, the index does
    // not follow the buffers. Files a session stored against the JDK alone
    // are resolved again when the settled world arrives, in one pass.
    let owed_now = !changed.is_empty()
        || worker
            .db
            .owed_member_refs(REFS_WITH_JDK)
            .is_ok_and(|owed| !owed.is_empty());
    let (early, taken) = if world_coming && owed_now {
        match worker.await_settled(jobs, started) {
            Some(early) => early,
            None => return,
        }
    } else {
        (Vec::new(), 0)
    };
    // A file an earlier session stored and never resolved — it was killed
    // before its pass finished — or resolved against another world, is owed
    // too, and this walk found it unchanged.
    match worker.db.owed_member_refs(worker.state()) {
        Ok(owed) => changed.extend(owed.into_iter().map(|path| (path, None))),
        Err(err) => tracing::warn!(%err, "workspace: owed member references unreadable"),
    }
    let gone: HashSet<PathBuf> = walked.gone.into_iter().collect();
    let resolving = Instant::now();
    let Ok(files) = worker.resolve_affected(changed, &gone, |path| {
        before.get(path).cloned().unwrap_or_default()
    }) else {
        return;
    };
    worker.publish(started);
    tracing::info!(
        files,
        settled = worker.settled.is_some(),
        duration_ms = resolving.elapsed().as_millis() as u64,
        "member references ready"
    );
    if !early.is_empty() {
        // Counted only now: a publish that claimed them before they were
        // applied would let the loop judge a buffer against an index that
        // does not hold it.
        worker.absorbed += taken;
        worker.apply(early);
    }

    // The walk is done; the worker stays, owning the store, and keeps the
    // index level with the workspace as the loop reports changes. It ends
    // when the session does — the loop's sender is dropped with the state —
    // or when the session is cancelled.
    //
    // ponytail: every publish reads the whole store back into a new index, so
    // its cost is the workspace's rather than the change's — a monorepo pays
    // that once per pause in typing. The upgrade is to rebuild only the
    // changed files' share of the in-memory maps.
    while let Ok(first) = jobs.recv() {
        let (batch, received) = gather(first, jobs, QUIET, LONGEST);
        if cancel.is_cancelled() {
            return;
        }
        // Owed to the loop with the next publish, even one a failure delays.
        worker.absorbed += received;
        worker.apply(batch);
    }
}

/// The worker's state between jobs: the store it owns, the index it last
/// published, and the JDK world member references are resolved against.
struct Worker<'a> {
    roots: &'a [PathBuf],
    config: &'a Config,
    cancel: &'a CancellationToken,
    results: &'a Sender<Workspace>,
    db: CacheDb,
    world: Arc<TypeWorld>,
    /// The key of `world` once the dependencies have settled.
    settled: Option<u64>,
    current: Arc<WorkspaceIndex>,
    /// What the last whole walk saw, published beside every index.
    survey: Arc<javelin_index::Survey>,
    /// Jobs taken in and not yet reported by a publish.
    absorbed: usize,
    /// Where a pass long enough to wait on says how far it has got.
    progress: crate::progress::Reporter,
    /// The pass whose references the next publish makes visible: it ends
    /// there, not when the pass does, or a reviewer asks in between.
    resolving: Option<crate::progress::Task>,
}

impl Worker<'_> {
    /// The `source_files.member_refs` state of what this worker resolves.
    fn state(&self) -> i64 {
        match self.settled {
            Some(key) => refs_settled(key),
            None if self.world.is_empty() => REFS_WITHOUT_WORLD,
            None => REFS_WITH_JDK,
        }
    }

    /// Take jobs until the settled world arrives — or [`SETTLE_WAIT`] after
    /// `started`, and [`JDK_WAIT`] while not even the JDK is in, since a pass
    /// against no world is a third whole pass — and answer the others,
    /// folded, for after the first pass, with how many jobs they were;
    /// `None` when the session is over.
    fn await_settled(
        &mut self,
        jobs: &Receiver<Job>,
        started: Instant,
    ) -> Option<(Vec<Job>, usize)> {
        let mut early = Batch::default();
        let mut taken = 0;
        while self.settled.is_none() {
            let wait = if self.world.is_empty() {
                JDK_WAIT
            } else {
                SETTLE_WAIT
            };
            let job = match jobs.recv_deadline(started + wait) {
                Ok(job) => job,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => break,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return None,
            };
            match job {
                Job::World(world, settled) => {
                    self.absorbed += 1;
                    self.world = world;
                    self.settled = settled;
                }
                job => {
                    taken += 1;
                    early.push(job);
                }
            }
        }
        Some((early.into_jobs(), taken))
    }

    /// One batch: re-walk if asked, re-index the changed files, re-resolve
    /// them and the files that depended on what they declared, publish once.
    fn apply(&mut self, batch: Vec<Job>) {
        let started = Instant::now();
        let mut changes: Vec<SourceChange> = Vec::with_capacity(batch.len());
        let mut changed: Vec<(PathBuf, Option<String>)> = Vec::new();
        let mut gone: HashSet<PathBuf> = HashSet::new();
        let mut before: HashMap<PathBuf, Vec<String>> = HashMap::new();
        let mut worlded = false;
        for job in batch {
            match job {
                Job::World(world, settled) => {
                    worlded = !world.is_empty() || settled.is_some();
                    self.world = world;
                    if settled.is_some() {
                        self.settled = settled;
                    }
                }
                Job::Rescan => {
                    // The incremental walk: every file whose time and size
                    // are stored is skipped, and every stored path the walk
                    // no longer finds is forgotten — which is what a deleted
                    // folder needs.
                    let (found, survey) = javelin_index::java_sources_surveyed(
                        self.roots,
                        self.config.index.max_file_size_bytes,
                        self.cancel,
                    );
                    self.db
                        .set_lombok_renamed(survey.lombok_renamed.iter().cloned());
                    self.db.set_lombok_log(survey.lombok_log.clone());
                    self.survey = Arc::new(survey);
                    match self.db.index_sources_reporting(&found, self.cancel) {
                        Ok(walked) => {
                            for path in walked.changed.iter().chain(&walked.gone) {
                                before
                                    .entry(path.clone())
                                    .or_insert_with(|| self.declared(path));
                            }
                            changed.extend(walked.changed.into_iter().map(|path| (path, None)));
                            gone.extend(walked.gone);
                            self.current = Arc::new(walked.index);
                        }
                        Err(err) => tracing::warn!(%err, "workspace: re-walk failed"),
                    }
                }
                // `or_insert_with`: a rescan earlier in the batch already
                // recorded what the path declared before it, and the index it
                // left describes after.
                Job::Disk(path) => {
                    before
                        .entry(path.clone())
                        .or_insert_with(|| self.declared(&path));
                    changed.push((path.clone(), None));
                    changes.push(SourceChange::Disk(path));
                }
                Job::Buffer(path, document) => {
                    before
                        .entry(path.clone())
                        .or_insert_with(|| self.declared(&path));
                    let text = document.text().to_owned();
                    changed.push((path.clone(), Some(text.clone())));
                    changes.push(SourceChange::Buffer { path, text });
                }
            }
        }
        match self
            .db
            .refresh(&changes, self.config.index.max_file_size_bytes, self.cancel)
        {
            Ok(index) => self.current = Arc::new(index),
            Err(err) => {
                tracing::warn!(%err, "workspace: re-indexing failed");
                return;
            }
        }
        // A path the refresh forgot — deleted, or grown past the size limit —
        // is gone: nothing to resolve in it, and its dependents are owed.
        let (changed, forgotten): (Vec<_>, Vec<_>) = changed
            .into_iter()
            .partition(|(path, _)| self.current.file_of(path).is_some());
        gone.extend(forgotten.into_iter().map(|(path, _)| path));
        let mut changed = changed;
        if worlded {
            // Resolved against a lesser world, here or in an earlier session:
            // a chain through a JDK or JAR return type answered nothing then.
            match self.db.owed_member_refs(self.state()) {
                Ok(owed) => changed.extend(owed.into_iter().map(|path| (path, None))),
                Err(err) => tracing::warn!(%err, "workspace: owed member references unreadable"),
            }
        }
        let resolving = Instant::now();
        let Ok(files) = self.resolve_affected(changed, &gone, |path| {
            before.get(path).cloned().unwrap_or_default()
        }) else {
            return;
        };
        self.publish(started);
        if files > 0 {
            tracing::debug!(
                files,
                duration_ms = resolving.elapsed().as_millis() as u64,
                "member references resolved"
            );
        }
    }

    /// The types `path` declared as of the last published index.
    fn declared(&self, path: &Path) -> Vec<String> {
        self.current
            .types_in(path)
            .map(|ty| ty.fqn.clone())
            .collect()
    }

    /// Resolve the member references of `changed`, and of every file whose
    /// references depended on a type `changed` or `gone` declared before
    /// or declares now, and store them.
    ///
    /// Stored a batch of files at a time, each marked resolved in the same
    /// transaction, so a session killed halfway leaves the rest pending for
    /// the next one rather than lost. Answers how many files it resolved.
    ///
    /// Resolved on every core but one: each file is parsed and walked alone,
    /// against a shared read-only index, and a whole workspace's first pass
    /// is what a reviewer waits on before references cross a file.
    ///
    /// ponytail: 6 701 files take 7.1 s on eleven threads, 19.9 s before
    /// members were shared behind an `Arc` — still allocation-bound, because
    /// `external_members` builds a `Member` for every member of a type on
    /// every lookup of one name. A lookup filtered by name is the upgrade.
    fn resolve_affected(
        &mut self,
        changed: Vec<(PathBuf, Option<String>)>,
        gone: &HashSet<PathBuf>,
        before: impl Fn(&Path) -> Vec<String>,
    ) -> Result<usize, Cancelled> {
        // Both the fully-qualified names and the simple ones: a file records
        // every name it wrote, resolved or not, so a type that appears can be
        // matched to the files that named it before it existed.
        let mut touched: BTreeSet<String> = BTreeSet::new();
        for path in changed
            .iter()
            .map(|(path, _)| path.as_path())
            .chain(gone.iter().map(PathBuf::as_path))
        {
            let after = self
                .current
                .types_in(path)
                .map(|ty| ty.fqn.clone())
                .collect();
            touch(&mut touched, before(path), after);
        }
        let touched: Vec<String> = touched.into_iter().collect();
        let mut listed: HashSet<PathBuf> = HashSet::with_capacity(changed.len());
        let mut targets: Vec<(PathBuf, Option<String>)> = Vec::with_capacity(changed.len());
        for (path, text) in changed {
            if listed.insert(path.clone()) {
                targets.push((path, text));
            }
        }
        match self.db.dependents(&touched) {
            Ok(dependents) => {
                for path in dependents {
                    if !gone.contains(&path)
                        && self.current.file_of(&path).is_some()
                        && listed.insert(path.clone())
                    {
                        // ponytail: a dependent that is an open, dirty buffer
                        // is re-resolved from disk, so its recorded uses are
                        // the saved text's until its next edit. The upgrade
                        // is the worker keeping each buffer's last text until
                        // a `Disk` job for its path, and resolving from that.
                        targets.push((path, None));
                    }
                }
            }
            Err(err) => tracing::warn!(%err, "workspace: dependents unreadable"),
        }
        if targets.is_empty() {
            return Ok(0);
        }
        let pending: Vec<PathBuf> = targets.iter().map(|(path, _)| path.clone()).collect();
        if let Err(err) = self.db.mark_member_refs_pending(&pending) {
            tracing::warn!(%err, "workspace: member references not marked pending");
        }
        let externals = crate::classpath::ClasspathExternals::new(
            Arc::clone(&self.world),
            Arc::clone(&self.current),
        );
        let state = self.state();
        let cancel = self.cancel;
        let threads = std::thread::available_parallelism()
            .map_or(1, |cores| cores.get().saturating_sub(1).max(1))
            .min(targets.len());
        let next = std::sync::atomic::AtomicUsize::new(0);
        let total = targets.len();
        // Only a pass long enough to wait on: an edit's few dependents would
        // flash an indicator on every pause in typing.
        let mut task = (total > STORED_PER_BATCH)
            .then(|| self.progress.begin("Resolving cross-file references"));
        std::thread::scope(|scope| {
            // Unbounded, so the threads never wait on the store's transactions;
            // inside the scope, so an early return drops the receiver and every
            // thread stops at its next send rather than resolving the rest.
            let (found_tx, found) = crossbeam_channel::unbounded();
            for _ in 0..threads {
                let found_tx = found_tx.clone();
                let (targets, next, externals) = (&targets, &next, &externals);
                scope.spawn(move || {
                    let Ok(mut parser) = JavaParser::new() else {
                        return;
                    };
                    loop {
                        let at = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some((path, text)) = targets.get(at) else {
                            return;
                        };
                        let resolved =
                            resolve_one(path, text.clone(), &mut parser, externals, cancel);
                        let stop = resolved.is_err();
                        if found_tx.send((path.clone(), resolved)).is_err() || stop {
                            return;
                        }
                    }
                });
            }
            drop(found_tx);
            let mut resolved = Vec::with_capacity(total.min(STORED_PER_BATCH));
            for (done, (path, found)) in found.iter().enumerate() {
                let found: crate::member_refs::Resolved = found?;
                if let Some(task) = &mut task {
                    task.step(done + 1, total, "files");
                }
                resolved.push((path, found.refs, found.deps));
                if resolved.len() == STORED_PER_BATCH {
                    self.store(&resolved, state);
                    resolved.clear();
                }
            }
            self.store(&resolved, state);
            self.resolving = task;
            Ok(total)
        })
    }

    fn store(
        &mut self,
        resolved: &[(PathBuf, Vec<javelin_index::MemberRef>, Vec<String>)],
        state: i64,
    ) {
        if resolved.is_empty() {
            return;
        }
        if let Err(err) = self.db.store_member_refs(resolved, state) {
            tracing::warn!(%err, "workspace: member references not stored");
        }
    }

    /// Read the store back and publish it.
    fn publish(&mut self, started: Instant) {
        match self.db.workspace_index() {
            Ok(index) => {
                self.current = Arc::new(index);
                self.send(started, true);
            }
            Err(err) => tracing::warn!(%err, "workspace: index unreadable"),
        }
        self.resolving = None;
    }

    fn send(&mut self, started: Instant, refreshed: bool) {
        let _ = self.results.send(Workspace {
            index: Arc::clone(&self.current),
            elapsed: started.elapsed(),
            refreshed,
            survey: Some(Arc::clone(&self.survey)),
            absorbed: std::mem::take(&mut self.absorbed),
        });
    }
}

/// One file's member references, read from disk unless its text is given. A
/// file that no longer reads — deleted with no event to say so — keeps
/// nothing it recorded before.
fn resolve_one(
    path: &Path,
    text: Option<String>,
    parser: &mut JavaParser,
    externals: &crate::classpath::ClasspathExternals,
    cancel: &CancellationToken,
) -> Result<crate::member_refs::Resolved, Cancelled> {
    cancel.check()?;
    let text = match text {
        Some(text) => Some(text),
        None => std::fs::read(path).ok().map(javelin_index::decode_source),
    };
    Ok(match text {
        Some(text) => crate::member_refs::resolve_file(path, text, parser, externals, cancel)?
            .unwrap_or_default(),
        None => crate::member_refs::Resolved::default(),
    })
}

fn open(config: &Config, workspace: &Path) -> Option<CacheDb> {
    let path = cache_path(config, workspace)
        .inspect_err(|err| tracing::warn!(%err, "workspace: no cache directory"))
        .ok()?;
    if let Ok(root) = javelin_core::paths::cache_root(config.index.cache_dir.as_deref())
        && let Err(err) = javelin_core::paths::mark_workspace_cache(&root, workspace)
    {
        tracing::debug!(%err, "workspace: cache not marked as used");
    }
    CacheDb::open(&path)
        .inspect_err(
            |err| tracing::warn!(path = %path.display(), %err, "workspace: cache unusable"),
        )
        .ok()
}

/// Delete the caches of other workspaces that are gone or long unused: every
/// git worktree opened is a cache of its own, and nothing else removes one.
fn prune(config: &Config, workspace: &Path) {
    let Ok(root) = javelin_core::paths::cache_root(config.index.cache_dir.as_deref()) else {
        return;
    };
    let pruned =
        javelin_core::paths::prune_workspace_caches(&root, workspace, std::time::SystemTime::now());
    for dir in &pruned {
        tracing::info!(dir = %dir.display(), "workspace: pruned an unused cache");
    }
}

/// What `doctor` reports about a workspace's source index.
#[derive(Debug)]
pub struct Report {
    pub cache: Option<PathBuf>,
    /// `None` when no index has been written for this workspace yet, which is
    /// also what an unreadable or incompatible one reports: in both cases the
    /// next session walks every file again.
    pub indexed: Option<(u64, u64)>,
}

pub fn report(config: &Config, workspace: &Path) -> Report {
    let cache = cache_path(config, workspace).ok();
    let indexed = cache.as_deref().and_then(counts);
    Report { cache, indexed }
}

fn counts(path: &Path) -> Option<(u64, u64)> {
    // An absent index is reported as absent rather than created here: asking
    // what is indexed should not leave a file behind.
    if !path.exists() {
        return None;
    }
    CacheDb::open(path).ok()?.source_counts().ok()
}

/// The folders the client named at `initialize`, as paths on this machine.
///
/// `workspaceFolders` first, because a client that sent it may have sent
/// several and `rootUri` then names only the first; `rootUri` is the older
/// single-folder spelling and `rootPath` the one before that, which some
/// clients still send alone. A client that sent none of the three opened a
/// file without a project, and gets no index rather than a guess at one.
pub(crate) fn roots(init_params: &serde_json::Value) -> Vec<PathBuf> {
    if let Some(folders) = init_params
        .get("workspaceFolders")
        .and_then(|f| f.as_array())
    {
        let paths: Vec<PathBuf> = folders
            .iter()
            .filter_map(|folder| folder.get("uri")?.as_str())
            .filter_map(path_of)
            .collect();
        if !paths.is_empty() {
            return paths;
        }
    }
    if let Some(uri) = init_params.get("rootUri").and_then(|uri| uri.as_str())
        && let Some(path) = path_of(uri)
    {
        return vec![path];
    }
    init_params
        .get("rootPath")
        .and_then(|path| path.as_str())
        .map(|path| vec![PathBuf::from(path)])
        .unwrap_or_default()
}

/// The path a `file:` URI names — the inverse of `handlers::definition`'s
/// `file_uri`, undoing the percent escapes and, on Windows, the slash in front
/// of the drive letter.
///
/// `None` for anything that is not a `file:` URI: a client may name an
/// `untitled:` or a remote folder, and neither is a directory this machine can
/// walk.
pub(crate) fn path_of(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    let mut bytes = encoded.bytes();
    let mut decoded = Vec::with_capacity(encoded.len());
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let hex: String = bytes.by_ref().take(2).map(char::from).collect();
            // Length checked as well as digits: `from_str_radix` is happy with
            // the one character a truncated escape at the end of the URI
            // leaves, and would decode `%2` as a byte nobody wrote.
            let byte = (hex.len() == 2)
                .then(|| u8::from_str_radix(&hex, 16).ok())
                .flatten()?;
            decoded.push(byte);
        } else {
            decoded.push(byte);
        }
    }
    let path = String::from_utf8(decoded).ok()?;
    let drive = path
        .strip_prefix('/')
        .filter(|rest| rest.as_bytes().get(1) == Some(&b':'));
    // Rebuilt from its components, so one file is spelled one way whatever the
    // URI wrote — a doubled slash, a `.`, and on Windows the separator itself,
    // which a URI writes as `/` and a walk's `join` as `\\`. The index keys its
    // rows by that spelling, and two spellings of a file are two rows.
    Some(
        std::path::Path::new(drive.unwrap_or(&path))
            .components()
            .collect(),
    )
}

/// A workspace index built by the real walk over real files, for the tests
/// across this crate that need one: a member closure, a cross-file jump.
#[cfg(test)]
pub(crate) mod testing {
    use std::path::PathBuf;
    use std::sync::Arc;

    use javelin_core::CancellationToken;
    use javelin_index::{CacheDb, WorkspaceIndex};

    /// A directory of Java files, removed when dropped.
    pub(crate) struct Sources {
        pub(crate) root: PathBuf,
        pub(crate) index: Arc<WorkspaceIndex>,
        db: Option<CacheDb>,
    }

    impl Sources {
        /// The path of a file this tree was built with.
        pub(crate) fn path(&self, relative: &str) -> PathBuf {
            // As the walk and `path_of` spell it: on Windows `join` keeps a `/`.
            self.root.join(relative).components().collect()
        }
    }

    impl Drop for Sources {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// [`indexed`], with every file's member selections resolved and stored
    /// as the worker does after its walk — against no JDK.
    pub(crate) fn indexed_with_member_refs(name: &str, files: &[(&str, &str)]) -> Sources {
        indexed_with_member_refs_over(name, files, Arc::new(javelin_classpath::TypeWorld::empty()))
    }

    /// The same, resolved against `world`.
    pub(crate) fn indexed_with_member_refs_over(
        name: &str,
        files: &[(&str, &str)],
        world: Arc<javelin_classpath::TypeWorld>,
    ) -> Sources {
        let mut sources = indexed(name, files);
        let externals =
            crate::classpath::ClasspathExternals::new(world, Arc::clone(&sources.index));
        let mut parser = javelin_parser::JavaParser::new().expect("grammar");
        let mut resolved = Vec::new();
        for (relative, text) in files {
            let path = sources.path(relative);
            let found = crate::member_refs::resolve_file(
                &path,
                (*text).to_owned(),
                &mut parser,
                &externals,
                &CancellationToken::new(),
            )
            .expect("not cancelled")
            .expect("parses");
            resolved.push((path, found.refs, found.deps));
        }
        let mut db = sources.db.take().expect("the store the index came from");
        db.store_member_refs(&resolved, javelin_index::REFS_WITHOUT_WORLD)
            .expect("stored");
        sources.index = Arc::new(db.workspace_index().expect("readable"));
        sources.db = Some(db);
        sources
    }

    /// Write `files` — `(relative path, text)` — under a fresh directory named
    /// for `name`, walk it and index it exactly as a session would.
    pub(crate) fn indexed(name: &str, files: &[(&str, &str)]) -> Sources {
        let root = std::env::temp_dir().join(format!(
            "javelin-server-sources-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        for (relative, text) in files {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("dirs");
            std::fs::write(&path, text).expect("write");
        }
        let found = javelin_index::java_sources(
            std::slice::from_ref(&root),
            1 << 20,
            &CancellationToken::new(),
        );
        let mut db = CacheDb::in_memory().expect("db");
        let index = db
            .index_sources(&found, &CancellationToken::new())
            .expect("indexing");
        Sources {
            root,
            index: Arc::new(index),
            db: Some(db),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn buffer(path: &str, text: &str) -> Job {
        Job::Buffer(
            PathBuf::from(path),
            Arc::new(javelin_document::TextDocument::new(
                javelin_document::DocumentId::new(format!("file://{path}")),
                1,
                text,
            )),
        )
    }

    fn described(jobs: &[Job]) -> Vec<String> {
        jobs.iter()
            .map(|job| match job {
                Job::Disk(path) => format!("disk {}", path.display()),
                Job::Buffer(path, document) => {
                    format!("buffer {} {}", path.display(), document.text())
                }
                Job::Rescan => "rescan".to_owned(),
                Job::World(..) => "world".to_owned(),
            })
            .collect()
    }

    #[test]
    fn a_path_is_spelled_one_way_whatever_the_uri_wrote() {
        // Rows are keyed by the path's text, so two spellings of one file are
        // two rows: the event's path must come out as the walk's would.
        assert_eq!(
            path_of("file:///w//src/./A.java"),
            Some(PathBuf::from("/w/src/A.java"))
        );
    }

    #[test]
    fn a_burst_of_jobs_inside_the_quiet_period_is_one_batch() {
        let (jobs, queued) = crossbeam_channel::unbounded();
        for n in 0..5 {
            jobs.send(buffer("/w/A.java", &n.to_string()))
                .expect("send");
        }
        jobs.send(Job::Disk(PathBuf::from("/w/B.java")))
            .expect("send");
        let first = queued.recv().expect("a job");
        let (batch, received) = gather(
            first,
            &queued,
            Duration::from_millis(20),
            Duration::from_secs(2),
        );
        assert_eq!(
            described(&batch),
            vec!["buffer /w/A.java 4", "disk /w/B.java"]
        );
        assert_eq!(
            received, 6,
            "every job folded is counted, not every one kept"
        );
    }

    #[test]
    fn a_stream_that_never_pauses_is_still_published_after_the_longest_wait() {
        let (jobs, queued) = crossbeam_channel::unbounded();
        let feeder = std::thread::spawn(move || {
            for n in 0..400 {
                if jobs.send(buffer("/w/A.java", &n.to_string())).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let first = queued.recv().expect("a job");
        let started = std::time::Instant::now();
        let (batch, _) = gather(
            first,
            &queued,
            Duration::from_millis(50),
            Duration::from_millis(100),
        );
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(batch.len(), 1);
        drop(queued);
        let _ = feeder.join();
    }

    #[test]
    fn a_rescan_absorbs_disk_reads_and_keeps_buffers() {
        let batch = coalesce(vec![
            buffer("/w/A.java", "a"),
            Job::Disk(PathBuf::from("/w/C.java")),
            Job::Rescan,
            Job::Disk(PathBuf::from("/w/B.java")),
        ]);
        assert_eq!(
            described(&batch),
            vec!["rescan", "buffer /w/A.java a", "disk /w/B.java"],
            "a disk read after the rescan still runs; one before it is the rescan's"
        );
    }

    /// The next published index that satisfies `settled`, within a few
    /// seconds: the worker publishes after every walk and every batch.
    fn published(
        results: &crossbeam_channel::Receiver<Workspace>,
        settled: impl Fn(&WorkspaceIndex) -> bool,
    ) -> Arc<WorkspaceIndex> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let workspace = results
                .recv_timeout(left)
                .expect("a publish before the deadline");
            if settled(&workspace.index) {
                return workspace.index;
            }
        }
    }

    fn refs_to(index: &WorkspaceIndex, greeter: &Path, text: &str, name: &str) -> Vec<String> {
        let start = text.find(&format!(" {name}(")).expect("the declaration") as u32 + 1;
        index
            .member_refs_to(&greeter.to_string_lossy(), start)
            .map(|found| {
                index
                    .file(found.file)
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn member_references_follow_a_change_to_the_file_they_depend_on() {
        let root =
            std::env::temp_dir().join(format!("javelin-worker-member-refs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let package = root.join("com").join("example");
        std::fs::create_dir_all(&package).expect("dirs");
        let greeter = package.join("Greeter.java");
        let before = "package com.example;\npublic class Greeter {\n    void greet() {}\n}\n";
        std::fs::write(&greeter, before).expect("write");
        std::fs::write(
            package.join("Caller.java"),
            "package com.example;\nclass Caller { void m(Greeter g) { g.greet(); } }\n",
        )
        .expect("write");

        // In memory: the test is about the worker, not the cache directory.
        let mut config = Config::default();
        config.index.persist = false;
        let (results_tx, results) = crossbeam_channel::unbounded();
        let (jobs, queued) = crossbeam_channel::unbounded();
        assert!(spawn(
            vec![root.clone()],
            &config,
            CancellationToken::new(),
            results_tx,
            queued,
            false,
            Default::default()
        ));

        published(&results, |index| {
            !refs_to(index, &greeter, before, "greet").is_empty()
        });

        // `greet` moves down its file: every recorded reference to it is keyed
        // by its old offset, and Caller — unchanged on disk — must be resolved
        // again because it depended on Greeter.
        let after = "package com.example;\npublic class Greeter {\n    void other() {}\n    void greet() {}\n}\n";
        std::fs::write(&greeter, after).expect("write");
        jobs.send(Job::Disk(greeter.clone())).expect("send");
        let index = published(&results, |index| {
            !refs_to(index, &greeter, after, "greet").is_empty()
        });
        assert_eq!(
            refs_to(&index, &greeter, after, "greet"),
            vec!["Caller.java"]
        );
        drop(jobs);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A tree of `files` under a fresh directory, and a worker over it — in
    /// memory unless `cache` names a cache directory.
    fn worker_over(
        name: &str,
        files: &[(&str, &str)],
        cache: Option<&Path>,
    ) -> (
        PathBuf,
        crossbeam_channel::Sender<Job>,
        crossbeam_channel::Receiver<Workspace>,
    ) {
        let root = tree(name, files);
        let (jobs, results) = spawn_over(&root, cache);
        (root, jobs, results)
    }

    fn tree(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("javelin-worker-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (relative, text) in files {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("dirs");
            std::fs::write(&path, text).expect("write");
        }
        root
    }

    fn config_for(cache: Option<&Path>) -> Config {
        let mut config = Config::default();
        match cache {
            Some(cache) => config.index.cache_dir = Some(cache.to_path_buf()),
            None => config.index.persist = false,
        }
        config
    }

    fn spawn_over(
        root: &Path,
        cache: Option<&Path>,
    ) -> (
        crossbeam_channel::Sender<Job>,
        crossbeam_channel::Receiver<Workspace>,
    ) {
        let (results_tx, results) = crossbeam_channel::unbounded();
        let (jobs, queued) = crossbeam_channel::unbounded();
        assert!(spawn(
            vec![root.to_path_buf()],
            &config_for(cache),
            CancellationToken::new(),
            results_tx,
            queued,
            false,
            Default::default()
        ));
        (jobs, results)
    }

    const GREETER: &str = "package com.example;\npublic class Greeter {\n    void greet() {}\n}\n";

    /// A buffer the editor sent while the worker waited for the classpath is
    /// applied after the first pass, and must not be reported absorbed by
    /// that pass's publish: the loop would judge it against an index that
    /// does not hold it yet.
    #[test]
    fn a_job_taken_while_waiting_is_absorbed_only_by_the_publish_that_applied_it() {
        let root = tree("absorbed-early", &[("com/example/Greeter.java", GREETER)]);
        let (results_tx, results) = crossbeam_channel::unbounded();
        let (jobs, queued) = crossbeam_channel::unbounded();
        assert!(spawn(
            vec![root.clone()],
            &config_for(None),
            CancellationToken::new(),
            results_tx,
            queued,
            true,
            Default::default()
        ));
        let greeter = root.join("com").join("example").join("Greeter.java");
        let document = Arc::new(javelin_document::TextDocument::new(
            javelin_document::DocumentId::new("file:///Greeter.java"),
            2,
            "package com.example;\npublic class Greeter {\n    void greet() {}\n    void wave() {}\n}\n"
                .to_owned(),
        ));
        jobs.send(Job::Buffer(greeter.clone(), document))
            .expect("send");
        jobs.send(Job::World(Arc::new(TypeWorld::empty()), Some(7)))
            .expect("send");
        let mut absorbed = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while absorbed.iter().sum::<usize>() < 2 {
            let left = deadline.saturating_duration_since(Instant::now());
            let workspace = results.recv_timeout(left).expect("a publish");
            let holds_wave = workspace
                .index
                .members_of("com.example.Greeter")
                .iter()
                .any(|member| member.name == "wave");
            absorbed.push(workspace.absorbed);
            // The world and the buffer: two jobs, and the buffer counted only
            // by a publish whose index holds it.
            assert!(
                holds_wave || absorbed.iter().sum::<usize>() < 2,
                "the buffer was claimed before it was applied: {absorbed:?}"
            );
        }
        drop(jobs);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A warm session with nothing owed does not hold the index back from
    /// the buffers for a classpath it does not need.
    #[test]
    fn a_session_with_nothing_owed_does_not_wait_for_the_classpath() {
        let root = tree(
            "nothing-owed",
            &[
                ("com/example/Greeter.java", GREETER),
                (
                    "com/example/Caller.java",
                    "package com.example;\nclass Caller { void m(Greeter g) { g.greet(); } }\n",
                ),
            ],
        );
        let cache = root.with_extension("cache");
        let _ = std::fs::remove_dir_all(&cache);
        let greeter = root.join("com").join("example").join("Greeter.java");
        let session = |send_world: bool| {
            let (results_tx, results) = crossbeam_channel::unbounded();
            let (jobs, queued) = crossbeam_channel::unbounded();
            assert!(spawn(
                vec![root.clone()],
                &config_for(Some(&cache)),
                CancellationToken::new(),
                results_tx,
                queued,
                true,
                Default::default()
            ));
            if send_world {
                jobs.send(Job::World(Arc::new(TypeWorld::empty()), Some(7)))
                    .expect("send");
            }
            (jobs, results)
        };
        let (jobs, results) = session(true);
        published(&results, |index| {
            !refs_to(index, &greeter, GREETER, "greet").is_empty()
        });
        drop(jobs);
        // Everything is stored against the settled world: the next session
        // publishes its resolution without being sent one.
        let (jobs, results) = session(false);
        let walked = results
            .recv_timeout(Duration::from_secs(10))
            .expect("the walk publishes");
        assert!(!walked.refreshed);
        let resolved = results
            .recv_timeout(Duration::from_secs(5))
            .expect("no wait for a classpath nothing needs");
        assert!(resolved.refreshed);
        drop(jobs);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// Resolving against the JDK alone and again once the JARs arrive is two
    /// whole passes: with a classpath coming, the first waits for it to settle.
    #[test]
    fn the_first_resolution_pass_waits_for_the_settled_classpath() {
        let root = tree(
            "settled",
            &[
                ("com/example/Greeter.java", GREETER),
                (
                    "com/example/Caller.java",
                    "package com.example;\nclass Caller { void m(Greeter g) { g.greet(); } }\n",
                ),
            ],
        );
        let (results_tx, results) = crossbeam_channel::unbounded();
        let (jobs, queued) = crossbeam_channel::unbounded();
        assert!(spawn(
            vec![root.clone()],
            &config_for(None),
            CancellationToken::new(),
            results_tx,
            queued,
            true,
            Default::default()
        ));
        let walked = results
            .recv_timeout(Duration::from_secs(10))
            .expect("the walk publishes");
        assert!(!walked.refreshed);
        assert!(
            results.recv_timeout(Duration::from_millis(500)).is_err(),
            "no resolution pass before the classpath settles"
        );
        jobs.send(Job::World(Arc::new(TypeWorld::empty()), Some(7)))
            .expect("send");
        let greeter = root.join("com").join("example").join("Greeter.java");
        published(&results, |index| {
            !refs_to(index, &greeter, GREETER, "greet").is_empty()
        });
        drop(jobs);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_naming_a_type_that_does_not_exist_yet_is_resolved_when_it_arrives() {
        let (root, jobs, results) = worker_over(
            "arrives",
            &[(
                "com/example/Caller.java",
                "package com.example;\nclass Caller { void m(Greeter g) { g.greet(); } }\n",
            )],
            None,
        );
        published(&results, |index| index.files() == 1);
        let greeter = root.join("com").join("example").join("Greeter.java");
        std::fs::write(&greeter, GREETER).expect("write");
        jobs.send(Job::Disk(greeter.clone())).expect("send");
        let index = published(&results, |index| {
            !refs_to(index, &greeter, GREETER, "greet").is_empty()
        });
        assert_eq!(
            refs_to(&index, &greeter, GREETER, "greet"),
            vec!["Caller.java"]
        );
        drop(jobs);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn deleting_a_file_drops_the_references_that_went_through_it() {
        let (root, jobs, results) = worker_over(
            "deleted",
            &[
                ("com/example/Greeter.java", GREETER),
                (
                    "com/example/Maker.java",
                    "package com.example;\npublic class Maker {\n    Greeter make() { return null; }\n}\n",
                ),
                (
                    "com/example/Caller.java",
                    "package com.example;\nclass Caller { void m(Maker k) { k.make().greet(); } }\n",
                ),
            ],
            None,
        );
        let greeter = root.join("com").join("example").join("Greeter.java");
        published(&results, |index| {
            !refs_to(index, &greeter, GREETER, "greet").is_empty()
        });
        let maker = root.join("com").join("example").join("Maker.java");
        std::fs::remove_file(&maker).expect("delete");
        jobs.send(Job::Disk(maker)).expect("send");
        // Caller's chain went through `make`'s return type, which is gone.
        published(&results, |index| {
            index.files() == 2 && refs_to(index, &greeter, GREETER, "greet").is_empty()
        });
        drop(jobs);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dependent_that_can_no_longer_be_read_loses_its_references() {
        let (root, jobs, results) = worker_over(
            "unreadable",
            &[
                ("com/example/Greeter.java", GREETER),
                (
                    "com/example/Caller.java",
                    "package com.example;\nclass Caller { void m(Greeter g) { g.greet(); } }\n",
                ),
            ],
            None,
        );
        let greeter = root.join("com").join("example").join("Greeter.java");
        published(&results, |index| {
            !refs_to(index, &greeter, GREETER, "greet").is_empty()
        });
        // Gone without an event, and Greeter changes below `greet`, so the
        // offset every reference is keyed by stays where it was.
        std::fs::remove_file(root.join("com").join("example").join("Caller.java")).expect("delete");
        let grown = "package com.example;\npublic class Greeter {\n    void greet() {}\n    void more() {}\n}\n";
        std::fs::write(&greeter, grown).expect("write");
        jobs.send(Job::Disk(greeter.clone())).expect("send");
        published(&results, |index| {
            refs_to(index, &greeter, grown, "greet").is_empty()
        });
        drop(jobs);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A session that stored the walk and was killed before the resolution
    /// pass stored anything: the next walk finds every file unchanged, and
    /// the references are still owed.
    #[test]
    fn a_file_an_earlier_session_left_unresolved_is_resolved_at_startup() {
        let root = tree(
            "unresolved-at-startup",
            &[
                ("com/example/Greeter.java", GREETER),
                (
                    "com/example/Caller.java",
                    "package com.example;\nclass Caller { void m(Greeter g) { g.greet(); } }\n",
                ),
            ],
        );
        let cache = root.with_extension("cache");
        let _ = std::fs::remove_dir_all(&cache);
        {
            let config = config_for(Some(&cache));
            let mut db =
                CacheDb::open(&cache_path(&config, &root).expect("a cache path")).expect("a store");
            let found = javelin_index::java_sources(
                std::slice::from_ref(&root),
                u64::MAX,
                &CancellationToken::new(),
            );
            db.index_sources(&found, &CancellationToken::new())
                .expect("indexing");
        }
        let (jobs, results) = spawn_over(&root, Some(&cache));
        let greeter = root.join("com").join("example").join("Greeter.java");
        published(&results, |index| {
            !refs_to(index, &greeter, GREETER, "greet").is_empty()
        });
        drop(jobs);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// A type present before and after a change cannot change what a
    /// spelling means, so only its fully-qualified name is matched — a
    /// keystroke in a file of generated `Builder`s must not re-resolve every
    /// file that writes `Builder`.
    #[test]
    fn only_a_type_that_appeared_or_disappeared_is_matched_by_simple_name() {
        let before = vec![
            "p.Msg".to_owned(),
            "p.Msg.Builder".to_owned(),
            "p.Old".to_owned(),
        ];
        let after = vec![
            "p.Msg".to_owned(),
            "p.Msg.Builder".to_owned(),
            "p.New".to_owned(),
        ];
        let mut touched = BTreeSet::new();
        touch(&mut touched, before, after);
        assert_eq!(
            touched.into_iter().collect::<Vec<_>>(),
            vec!["New", "Old", "p.Msg", "p.Msg.Builder", "p.New", "p.Old"]
        );
    }

    #[test]
    fn coalescing_keeps_the_last_job_per_path_in_arrival_order() {
        let jobs = vec![
            buffer("/w/A.java", "one"),
            buffer("/w/B.java", "b"),
            buffer("/w/A.java", "two"),
            Job::Disk(PathBuf::from("/w/B.java")),
        ];
        assert_eq!(
            described(&coalesce(jobs)),
            vec!["buffer /w/A.java two", "disk /w/B.java"]
        );
    }

    #[test]
    fn workspace_folders_win_over_a_root_uri() {
        let params = json!({
            "workspaceFolders": [{"uri": "file:///one", "name": "one"},
                                 {"uri": "file:///two", "name": "two"}],
            "rootUri": "file:///three",
        });
        assert_eq!(
            roots(&params),
            vec![PathBuf::from("/one"), PathBuf::from("/two")]
        );
    }

    #[test]
    fn a_root_uri_is_taken_when_no_folders_were_sent() {
        let params = json!({ "rootUri": "file:///project" });
        assert_eq!(roots(&params), vec![PathBuf::from("/project")]);
    }

    #[test]
    fn a_null_workspace_folders_falls_through_to_the_root_uri() {
        let params = json!({ "workspaceFolders": null, "rootUri": "file:///project" });
        assert_eq!(roots(&params), vec![PathBuf::from("/project")]);
    }

    #[test]
    fn an_empty_workspace_folders_falls_through_to_the_root_uri() {
        let params = json!({ "workspaceFolders": [], "rootUri": "file:///project" });
        assert_eq!(roots(&params), vec![PathBuf::from("/project")]);
    }

    #[test]
    fn a_client_that_named_nothing_gets_no_roots() {
        assert!(roots(&json!({})).is_empty());
    }

    #[test]
    fn a_folder_that_is_not_on_this_machine_is_not_a_root() {
        let params = json!({ "workspaceFolders": [{"uri": "untitled:Untitled-1"}] });
        assert!(roots(&params).is_empty());
    }

    #[test]
    fn a_percent_escape_in_a_folder_name_is_decoded() {
        let params = json!({ "rootUri": "file:///home/a%20user/pro%C3%9Fect" });
        assert_eq!(roots(&params), vec![PathBuf::from("/home/a user/proßect")]);
    }

    #[test]
    fn a_windows_drive_letter_loses_the_uris_leading_slash() {
        assert_eq!(
            path_of("file:///C:/code/project"),
            Some(PathBuf::from("C:/code/project"))
        );
    }

    #[test]
    fn a_truncated_percent_escape_is_refused_rather_than_guessed_at() {
        assert_eq!(path_of("file:///home/a%2"), None);
    }
}
