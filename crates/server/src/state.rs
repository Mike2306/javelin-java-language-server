//! The server's mutable state and the handlers that touch it.
//!
//! Kept separate from the transport loop in `lib.rs` so it can be driven
//! directly from tests without a pipe and a JSON-RPC framing layer.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use javelin_classpath::TypeWorld;
use javelin_core::{CancellationToken, Config};
use javelin_document::{Change, DocumentId, DocumentStore, EditBatch, Range, TextDocument};
use javelin_index::WorkspaceIndex;
use javelin_parser::{FileSyntax, JavaParser, ScopeTree, SyntaxTree};
use javelin_protocol::{EngineState, Readiness};

use crate::classpath::{Classpath, ClasspathExternals};
use crate::handlers::to_position;
use crate::jdk_sources::JdkSources;
use crate::pool::{Job, Outcome, Pool};
use crate::workspace::{Job as IndexJob, Workspace};

/// How long [`ServerState::settle`] waits before giving up on the pool.
///
/// Only reachable if a worker died holding a job, which is a bug; the point is
/// that a test or benchmark fails with its own assertion instead of hanging.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything derived from one document version, and the snapshot it came from.
///
/// The snapshot is owned here rather than looked up on demand, which is what
/// makes the four pieces agree *by construction*. They used to live in three
/// separate maps beside a document the store could have moved on from, so a
/// reparse that had not landed yet produced ranges computed from new text
/// against an old tree — wrong answers rather than old ones.
#[derive(Debug)]
pub(crate) struct Analysis {
    document: Arc<TextDocument>,
    tree: SyntaxTree,
    /// Built on first ask, or eagerly by a worker. Lazy on the `didOpen` path
    /// because nothing should pay for a view of a file no one asked about, and
    /// because a first parse is the whole of the "file becomes useful" promise.
    syntax: Option<FileSyntax>,
    scopes: Option<ScopeTree>,
}

impl Analysis {
    pub(crate) fn new(document: Arc<TextDocument>, tree: SyntaxTree) -> Self {
        Self {
            document,
            tree,
            syntax: None,
            scopes: None,
        }
    }

    pub(crate) fn version(&self) -> i32 {
        self.document.version()
    }

    pub(crate) fn document(&self) -> &TextDocument {
        &self.document
    }

    /// Build both derived views, so the request that needs them does not.
    pub(crate) fn warm(&mut self) {
        let _ = self.syntax();
        if self.scopes.is_none() {
            self.scopes = Some(ScopeTree::build(&self.document, &self.tree));
        }
    }

    fn syntax(&mut self) -> Option<&FileSyntax> {
        if self.syntax.is_none() {
            self.syntax = Some(FileSyntax::build(&self.document, &self.tree));
        }
        self.syntax.as_ref()
    }

    fn view(&mut self) -> Option<FileView<'_>> {
        self.warm();
        Some(FileView {
            document: Arc::clone(&self.document),
            tree: &self.tree,
            syntax: self.syntax.as_ref()?,
            scopes: self.scopes.as_ref()?,
        })
    }

    #[cfg(test)]
    pub(crate) fn is_warm(&self) -> bool {
        self.syntax.is_some() && self.scopes.is_some()
    }
}

/// Everything a question about one file is answered from, all describing the
/// same document version.
///
/// That version is not necessarily the newest one: while a reparse is in flight
/// this is the previous view, which is the honest partial answer. Compare
/// `document.version()` against the document store to tell.
#[derive(Debug)]
pub struct FileView<'a> {
    pub document: Arc<TextDocument>,
    pub tree: &'a SyntaxTree,
    pub syntax: &'a FileSyntax,
    pub scopes: &'a ScopeTree,
}

/// A reparse the pool has been given and has not answered yet.
#[derive(Debug)]
struct Pending {
    seq: u64,
    token: CancellationToken,
}

/// Everything the server knows.
///
/// There is deliberately no global: the state is created in `run` and threaded
/// through, so tests can spin up as many independent servers as they like and
/// nothing leaks between them. The parse pool is a field of it for the same
/// reason — workers are owned by one server and joined when it is dropped.
#[derive(Debug)]
pub struct ServerState {
    pub config: Config,
    pub documents: DocumentStore,
    pub engine: EngineState,
    /// The `didOpen` parser, used on the loop thread. Workers have their own.
    parser: JavaParser,
    analyses: HashMap<DocumentId, Analysis>,
    pool: Pool,
    /// The live reparse per document. At most one: dispatching a new one
    /// cancels its predecessor.
    pending: HashMap<DocumentId, Pending>,
    next_seq: u64,
    /// Jobs dispatched and not yet answered, including superseded ones. Every
    /// job answers exactly once, so this reaches zero.
    outstanding: usize,
    /// Empty until the classpath job lands one, and on a machine with no JDK
    /// for the whole session. Shared with every request rather than copied:
    /// it is immutable from the moment it is built.
    world: Arc<TypeWorld>,
    /// Where a jump into the JDK's own sources reads its file from. Arrives
    /// with the world and stays `None` without one — a JDK with no `src.zip`,
    /// a hand-built classpath, or a cache root that cannot be resolved, each
    /// of which costs the jump and nothing else.
    jdk_sources: Option<JdkSources>,
    /// The `src.zip` and version `jdk_sources` was opened for, so a second
    /// publish of the same JDK — the one that adds the dependencies — keeps
    /// the archive it already opened rather than paying for opening it again.
    jdk_sources_for: Option<(std::path::PathBuf, String)>,
    /// Each dependency's `-sources.jar` a jump has gone into, opened on the
    /// first and kept, as `jdk_sources` keeps the JDK's.
    jar_sources: HashMap<std::path::PathBuf, JdkSources>,
    classpath: Receiver<Classpath>,
    /// The reader's end, cloned into the job — and kept here even once it has
    /// been, exactly as the pool keeps its own: without it, the reader thread
    /// ending would disconnect the channel and turn the loop's `select!` arm
    /// into a spin.
    classpath_sender: Sender<Classpath>,
    /// Stops the reader mid-`src.zip` when the session ends. Nothing ever
    /// *waits* for it to notice — see [`crate::classpath::spawn`].
    classpath_cancel: CancellationToken,
    /// The build tool the classpath thread is running, if any: killed here at
    /// shutdown, because that thread is never joined and a process that exits
    /// leaves its children running.
    build_child: javelin_build::ChildSlot,
    /// One read per session. The loop starts it once, so this defends the
    /// invariant against a second caller — a `serve` that ever restarts its
    /// handshake, or a test driving the state directly — rather than against
    /// a client.
    classpath_started: bool,
    /// The classpath thread is running, so a settled world will arrive.
    classpath_running: bool,
    /// Empty until the workspace walk lands one, and for the whole session
    /// where the client named no folder. Shared as an `Arc` for the same
    /// reason the world is: immutable from the moment it is built.
    workspace: Arc<WorkspaceIndex>,
    workspace_results: Receiver<Workspace>,
    /// Kept for the reason `classpath_sender` is: without it, the indexing
    /// thread ending would disconnect the channel and spin the loop's arm.
    workspace_sender: Sender<Workspace>,
    /// Where the background workers say what they are doing; the loop shows
    /// it to a client that shows progress.
    progress_results: Receiver<crate::progress::Progress>,
    progress_sender: Sender<crate::progress::Progress>,
    workspace_cancel: CancellationToken,
    workspace_started: bool,
    /// Where a file's change goes to be re-indexed, once the worker is up —
    /// `None` for a session that indexes nothing.
    workspace_jobs: Option<Sender<IndexJob>>,
    /// The folders the client named, which decide whether a path is the
    /// workspace's at all.
    roots: Vec<std::path::PathBuf>,
    /// Files whose buffer text has been handed to the index since they were
    /// last read from disk — the only ones a close has to put back.
    buffered: std::collections::HashSet<std::path::PathBuf>,
    /// The diagnostics worker, started the first time a document can be
    /// judged; `None` until then and for a session that never can.
    diagnostics_jobs: Option<Sender<crate::diagnostics::Job>>,
    diagnostics_results: Receiver<crate::diagnostics::Published>,
    /// Kept for the reason `classpath_sender` is.
    diagnostics_sender: Sender<crate::diagnostics::Published>,
    /// How the dependencies came out, once the classpath thread has said.
    dependency_report: Option<crate::classpath::DependencyReport>,
    /// What the workspace walk saw besides the files it took.
    survey: Option<Arc<javelin_index::Survey>>,
    /// The client was asked to watch the workspace's files.
    watching: bool,
    /// Jobs handed to the index worker that no publish has reported taking
    /// in: while any is outstanding the index may be behind an open buffer.
    index_behind: usize,
    /// A build file changed after the classpath was read.
    build_changed: bool,
    /// The classpath, or a workspace file's imports, name a tool that adds
    /// members to declared types — see `member_rewriter`.
    rewriter_on_classpath: bool,
    rewriter_imported: bool,
    /// What `inlayHint` worked out per open document, so a request its
    /// deadline cut short is resumed by the next rather than redone.
    hint_memos: HashMap<DocumentId, crate::handlers::inlay_hint::Memo>,
    /// A hint request was cut short with work left: ask the client again.
    hint_refresh: bool,
    /// A refresh was sent and not answered yet; one at a time, under one id.
    hint_refresh_sent: bool,
    /// Documents whose hints were asked for before the view of their current
    /// version was installed: a refresh is owed when it is.
    hints_await_parse: std::collections::HashSet<DocumentId>,
    /// The client said it takes `workspace/inlayHint/refresh`.
    hint_refresh_support: bool,
}

/// Tools that add members to a type a source declares, by the package their
/// annotations live in: Manifold's extensions, which add methods to *any*
/// receiver. Not Lombok, whose annotations each type's index row answers
/// for — `javelin_classpath::LombokUse`.
///
/// ponytail: a hand-written list, so a member generator not on it is
/// invisible; `javelin_build::BuildHints::rewrites` is the same list read
/// from build files.
const MEMBER_REWRITERS: [&str; 1] = ["manifold"];

fn member_rewriter(name: &str) -> bool {
    MEMBER_REWRITERS.iter().any(|package| {
        name.strip_prefix(package)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
    })
}

/// Whether a build naming Java levels `low` to `high` is one a JDK of
/// feature version `jdk` answers for: none newer than it, whose types it
/// lacks, and none before 11 against a JDK after it, which dropped
/// `javax.xml.bind` and the rest of Java EE.
fn compiles_as_written((low, high): (u32, u32), jdk: u32) -> bool {
    high <= jdk && !(low <= 10 && jdk >= 11)
}

impl ServerState {
    pub fn new(config: Config) -> javelin_core::Result<Self> {
        // Before the pool: a broken grammar must fail startup here rather than
        // on every worker thread separately.
        let parser = JavaParser::new()?;
        let pool = Pool::new(config.workers.resolved_background_threads());
        let (classpath_tx, classpath) = crossbeam_channel::unbounded();
        let (workspace_tx, workspace) = crossbeam_channel::unbounded();
        let (diagnostics_tx, diagnostics) = crossbeam_channel::unbounded();
        let (progress_tx, progress) = crossbeam_channel::unbounded();
        Ok(Self {
            config,
            documents: DocumentStore::new(),
            engine: EngineState {
                readiness: Readiness::Starting,
            },
            parser,
            analyses: HashMap::new(),
            pool,
            pending: HashMap::new(),
            next_seq: 0,
            outstanding: 0,
            world: Arc::new(TypeWorld::empty()),
            jdk_sources: None,
            jdk_sources_for: None,
            jar_sources: HashMap::new(),
            classpath,
            classpath_sender: classpath_tx,
            classpath_cancel: CancellationToken::new(),
            build_child: javelin_build::ChildSlot::default(),
            classpath_started: false,
            classpath_running: false,
            workspace: Arc::new(WorkspaceIndex::default()),
            workspace_results: workspace,
            workspace_sender: workspace_tx,
            progress_results: progress,
            progress_sender: progress_tx,
            workspace_cancel: CancellationToken::new(),
            workspace_started: false,
            workspace_jobs: None,
            roots: Vec::new(),
            buffered: std::collections::HashSet::new(),
            diagnostics_jobs: None,
            diagnostics_results: diagnostics,
            diagnostics_sender: diagnostics_tx,
            dependency_report: None,
            survey: None,
            watching: false,
            index_behind: 0,
            build_changed: false,
            rewriter_on_classpath: false,
            rewriter_imported: false,
            hint_memos: HashMap::new(),
            hint_refresh: false,
            hint_refresh_sent: false,
            hints_await_parse: std::collections::HashSet::new(),
            hint_refresh_support: false,
        })
    }

    pub(crate) fn diagnostics_results(&self) -> Receiver<crate::diagnostics::Published> {
        self.diagnostics_results.clone()
    }

    /// Whether a judgement is still about what the editor shows: its document
    /// is open at the version judged. A clear always is.
    ///
    /// The very snapshot, not only its version: a document closed and opened
    /// again starts its versions over, and a judgement of the old text would
    /// otherwise pass for one of the new.
    pub(crate) fn is_current(&self, published: &crate::diagnostics::Published) -> bool {
        match &published.document {
            None => true,
            Some(judged) => self
                .documents
                .get(&DocumentId::new(published.uri.as_str()))
                .is_some_and(|document| Arc::ptr_eq(&document, judged)),
        }
    }

    /// The client watches the workspace's files for this server, so a
    /// `git checkout` reaches the index — without it a type a branch added is
    /// unknown until its file is saved in the editor.
    pub(crate) fn watching_files(&mut self) {
        self.watching = true;
    }

    /// The client said no after all: what was judged on the promise is
    /// withdrawn.
    pub(crate) fn watcher_refused(&mut self) {
        self.watching = false;
        self.judge_all();
    }

    /// The feature version of the JDK the world was read from: `21` for
    /// `21.0.2`, `8` for `1.8.0_392`.
    fn jdk_feature(&self) -> Option<u32> {
        let (_, version) = self.jdk_sources_for.as_ref()?;
        let mut parts = version.split(['.', '_', '-', '+']);
        match parts.next()? {
            "1" => parts.next()?.parse().ok(),
            first => first.parse().ok(),
        }
    }

    /// The world, if it is whole enough to judge a file against — the JDK
    /// in, the dependencies complete, the workspace walked with nothing left
    /// out — or `None`, in which case no file is judged at all.
    ///
    /// Besides: the client watches the files, the size limit is the default
    /// — the JDK's and the JARs' readers drop an entry over it without saying
    /// so, and at the default no JDK source is that large — a build that
    /// generates sources has generated some, and no Java level the build
    /// names is newer than this JDK or before 11 against one after it. What
    /// a JDK after 11 removed later is exempted name by name, in
    /// `diagnostics::REMOVED`.
    ///
    /// ponytail: "whole" is as of the last walk, so a file grown past the
    /// size limit or a Kotlin source added since is noticed at the next
    /// re-walk; and a level set outside the workspace — a parent POM, a
    /// published convention plugin — is not seen, so a project newer than
    /// the JDK is judged against it. The upgrades are re-surveying on every
    /// re-index and the build tool printing the level it compiles at.
    fn diagnosable(&self) -> Option<Arc<crate::diagnostics::Snapshot>> {
        if !self.config.diagnostics.enabled
            || !self.world.has_platform()
            || !self.watching
            || self.build_changed
            || self.config.index.max_file_size_bytes
                < javelin_core::Config::default().index.max_file_size_bytes
        {
            return None;
        }
        let report = self
            .dependency_report
            .as_ref()
            .filter(|report| report.complete)?;
        let survey = self
            .survey
            .as_ref()
            .filter(|survey| !survey.skipped_large && !survey.foreign && !survey.hidden)?;
        if report.codegen && survey.generated.is_empty() && !survey.generator_output {
            return None;
        }
        if let Some(release) = report.release
            && !compiles_as_written(release, self.jdk_feature()?)
        {
            return None;
        }
        Some(Arc::new(crate::diagnostics::Snapshot {
            externals: self.externals(),
            modules: report.modules.clone(),
            survey: Arc::clone(survey),
            processors: report.processors,
            members: !report.rewrites
                && !self.rewriter_on_classpath
                && !self.rewriter_imported
                && self.index_behind == 0,
            roots: self.roots.clone(),
            // A loose project is compiled by this JDK; a build that names no
            // level may compile at any, so every removal is exempted.
            level: match report.release {
                _ if report.loose => u32::MAX,
                Some((low, _)) => low,
                None => 0,
            },
            // Unknown, it is newer than any level: every JDK receiver is
            // refused.
            jdk: self.jdk_feature().unwrap_or(u32::MAX),
        }))
    }

    /// Hand `id` to the diagnostics worker, if the world can judge it.
    ///
    /// A world that stopped being whole — a re-walk met an oversized file —
    /// clears what was published instead, or a squiggle judged before would
    /// outlive the fix to the name under it.
    fn judge(&mut self, id: &DocumentId) {
        match self.diagnosable() {
            Some(snapshot) => self.judge_with(id, snapshot),
            None => self.clear(id),
        }
    }

    fn clear(&self, id: &DocumentId) {
        if let Some(jobs) = &self.diagnostics_jobs {
            let _ = jobs.send(crate::diagnostics::Job::Clear {
                uri: id.as_str().to_owned(),
            });
        }
    }

    /// Hand every open document to the diagnostics worker: the world changed.
    fn judge_all(&mut self) {
        let snapshot = self.diagnosable();
        let ids: Vec<DocumentId> = self
            .documents
            .iter()
            .map(|document| document.id().clone())
            .collect();
        for id in ids {
            match &snapshot {
                Some(snapshot) => self.judge_with(&id, Arc::clone(snapshot)),
                None => self.clear(&id),
            }
        }
    }

    fn judge_with(&mut self, id: &DocumentId, snapshot: Arc<crate::diagnostics::Snapshot>) {
        let Some(document) = self.documents.get(id) else {
            return;
        };
        // Only a file the walk would index is judged: one outside every root,
        // under a pruned directory or extracted out of `src.zip` names types
        // from a classpath this world does not describe.
        let Some(path) = self.admitted(id.as_str()) else {
            return;
        };
        // Nor a processor's output, which the walk indexes for navigation:
        // it is the build's, and judging it would judge the build. Admitted
        // past a directory the walk prunes is exactly that output — a
        // hand-written package `build.generated` below `src/` is not.
        if !javelin_index::within(&self.roots, &path) {
            return;
        }
        if self.diagnostics_jobs.is_none() {
            self.diagnostics_jobs = crate::diagnostics::spawn(
                std::time::Duration::from_millis(self.config.diagnostics.debounce_ms),
                self.diagnostics_sender.clone(),
            );
        }
        if let Some(jobs) = &self.diagnostics_jobs {
            let _ = jobs.send(crate::diagnostics::Job::Judge {
                uri: id.as_str().to_owned(),
                path,
                document,
                snapshot,
            });
        }
    }

    /// What the open file cannot see, as much of it as has arrived: the JDK's
    /// facts and the workspace's own sources, behind one door.
    ///
    /// Taken by value so a handler can hold it across the `&mut self` call for
    /// the document view it asks alongside; the clone is two refcount bumps.
    pub fn externals(&self) -> ClasspathExternals {
        ClasspathExternals::new(Arc::clone(&self.world), Arc::clone(&self.workspace))
    }

    /// Whether every answer this server gives is as good as it will get.
    ///
    /// The classpath is the last input any of the four features takes, so
    /// [`Readiness::Dependencies`] is where an answer stops being partial.
    /// Deliberately not `EngineState::supports`: every feature that exists is
    /// available from [`Readiness::File`] already, so `supports` would call a
    /// Phase 2 answer complete.
    pub fn is_complete(&self) -> bool {
        self.engine.readiness >= Readiness::Dependencies
    }

    /// The editor finished its side of the handshake. Start reading the JDK.
    ///
    /// Never called from `initialize`: that request is answered before this
    /// server exists, and nothing on the path to it may look at a filesystem
    /// for a JDK.
    pub fn start_classpath(&mut self, roots: Vec<std::path::PathBuf>) {
        if std::mem::replace(&mut self.classpath_started, true) {
            tracing::debug!("classpath: already started, ignoring a second request");
            return;
        }
        self.classpath_running = crate::classpath::spawn(
            &self.config,
            roots,
            self.classpath_cancel.clone(),
            std::sync::Arc::clone(&self.build_child),
            self.classpath_sender.clone(),
            crate::progress::Reporter::new(self.progress_sender.clone()),
        );
    }

    /// The editor finished its side of the handshake. Start walking the
    /// workspace's own sources.
    ///
    /// Separate from the classpath's start for the same reason they are
    /// separate threads: a JDK read and a workspace walk share nothing, and
    /// either one arriving is useful without the other.
    pub fn start_workspace(&mut self, roots: Vec<std::path::PathBuf>) {
        if std::mem::replace(&mut self.workspace_started, true) {
            tracing::debug!("workspace: already started, ignoring a second request");
            return;
        }
        let (jobs, queued) = crossbeam_channel::unbounded();
        if crate::workspace::spawn(
            roots.clone(),
            &self.config,
            self.workspace_cancel.clone(),
            self.workspace_sender.clone(),
            queued,
            self.classpath_running,
            crate::progress::Reporter::new(self.progress_sender.clone()),
        ) {
            self.workspace_jobs = Some(jobs);
            self.roots = roots;
        }
    }

    pub(crate) fn progress_results(&self) -> Receiver<crate::progress::Progress> {
        self.progress_results.clone()
    }

    pub(crate) fn workspace_results(&self) -> Receiver<Workspace> {
        self.workspace_results.clone()
    }

    /// Publish a finished workspace index. A field write and a log line; the
    /// walk happened on a thread that is not this one.
    ///
    /// `Readiness` is deliberately untouched, and now that three features
    /// read this index that is a choice rather than an omission. The ladder
    /// already carries two independent facts and is `ponytail:`-marked for it
    /// in `install_classpath`; a third claim on one axis would say even less
    /// about which of the three inputs an answer was waiting for.
    ///
    /// ponytail: so a definition or hover that found nothing is reported
    /// `Complete` once the *classpath* is in, even where the workspace walk
    /// has not landed and would have resolved the name. An overclaim on the
    /// seam and nowhere else — LSP has no field for it on either response, so
    /// nothing on the wire reads it, and the only consumer is a test. The
    /// upgrade is a "the walk has settled" flag that is also true for a client
    /// that named no folder, and it is worth writing when something on the
    /// wire can act on the difference.
    pub(crate) fn install_workspace(&mut self, workspace: Workspace) {
        if workspace.refreshed {
            tracing::debug!(
                files = workspace.index.files(),
                types = workspace.index.len(),
                duration_ms = workspace.elapsed.as_millis() as u64,
                "workspace re-indexed"
            );
        } else {
            tracing::info!(
                files = workspace.index.files(),
                types = workspace.index.len(),
                duration_ms = workspace.elapsed.as_millis() as u64,
                "workspace ready"
            );
        }
        self.workspace = workspace.index;
        self.index_behind = self.index_behind.saturating_sub(workspace.absorbed);
        self.rewriter_imported = self.workspace.sources().iter().any(|file| {
            file.imports
                .iter()
                .any(|import| member_rewriter(&import.name))
        });
        if let Some(survey) = workspace.survey {
            self.survey = Some(survey);
        }
        self.judge_all();
    }

    /// What the workspace's own sources declare, as much of it as has arrived.
    /// What the workspace walk saw besides the files it took — `None` until
    /// its first publish.
    pub(crate) fn survey(&self) -> Option<Arc<javelin_index::Survey>> {
        self.survey.clone()
    }

    /// The modules the build answered for, when every dependency the build
    /// names is in the world and no build file has changed since it was
    /// asked — `Some(None)` for a build with one classpath. `None` while a
    /// JAR may hold a name the world does not, which can outrank what a name
    /// resolves to now.
    pub(crate) fn whole_dependencies(&self) -> Option<Option<Arc<crate::modules::Modules>>> {
        let report = self
            .dependency_report
            .as_ref()
            .filter(|report| report.complete)?;
        (!self.build_changed).then(|| report.modules.clone())
    }

    pub fn workspace(&self) -> Arc<WorkspaceIndex> {
        Arc::clone(&self.workspace)
    }

    /// What a hint is read against besides its document, held weakly: an
    /// answer worked out against one is stale once any is replaced, and a
    /// weak pointer's allocation outlives what it pointed at, so a new world
    /// is never mistaken for the old one at the same address.
    pub(crate) fn hint_inputs(&self) -> crate::handlers::inlay_hint::Inputs {
        crate::handlers::inlay_hint::Inputs::new(
            Arc::downgrade(&self.world),
            Arc::downgrade(&self.workspace),
            self.survey.as_ref().map(Arc::downgrade),
        )
    }

    /// The hints worked out so far for `id`, taken out while a request adds
    /// to them.
    pub(crate) fn take_hint_memo(&mut self, id: &DocumentId) -> crate::handlers::inlay_hint::Memo {
        self.hint_memos.remove(id).unwrap_or_default()
    }

    pub(crate) fn put_hint_memo(
        &mut self,
        id: DocumentId,
        memo: crate::handlers::inlay_hint::Memo,
    ) {
        if self.documents.get(&id).is_some() {
            self.hint_memos.insert(id, memo);
        }
    }

    /// A hint request stopped at its deadline having made progress: the
    /// client is asked to ask again, if it takes being asked.
    pub(crate) fn owe_hint_refresh(&mut self) {
        self.hint_refresh |= self.hint_refresh_support;
    }

    /// Hints for `id` were asked for while its view lagged the buffer: owe
    /// a refresh once a view of the buffer's version is installed, not now,
    /// or the client would ask again while the reparse still runs.
    pub(crate) fn owe_hint_refresh_after_parse(&mut self, id: DocumentId) {
        if self.hint_refresh_support {
            self.hints_await_parse.insert(id);
        }
    }

    /// Whether to send a `workspace/inlayHint/refresh` now: one is owed and
    /// none is waiting for its answer.
    pub(crate) fn take_hint_refresh(&mut self) -> bool {
        let send = self.hint_refresh && !self.hint_refresh_sent;
        if send {
            self.hint_refresh = false;
            self.hint_refresh_sent = true;
        }
        send
    }

    pub(crate) fn hint_refresh_answered(&mut self) {
        self.hint_refresh_sent = false;
    }

    pub(crate) fn refreshes_hints(&mut self) {
        self.hint_refresh_support = true;
    }

    /// The channel the finished classpath arrives on, for the loop to select
    /// over next to the parse pool's.
    pub(crate) fn classpath_results(&self) -> Receiver<Classpath> {
        self.classpath.clone()
    }

    /// Publish a finished classpath. A map-free field write and a max: the
    /// reading took eleven seconds on a thread that is not this one.
    pub(crate) fn install_classpath(&mut self, classpath: Classpath) {
        tracing::info!(
            facts = classpath.world.len(),
            source = classpath.source,
            duration_ms = classpath.elapsed.as_millis() as u64,
            "classpath ready"
        );
        let settled = classpath.dependencies.as_ref().map(|report| report.key);
        if let Some(report) = classpath.dependencies {
            self.dependency_report = Some(report);
        }
        // The thread's last message resends the JDK's world when the
        // dependencies added nothing; the worker has already resolved against it.
        let changed = !Arc::ptr_eq(&self.world, &classpath.world);
        self.world = classpath.world;
        self.rewriter_on_classpath = MEMBER_REWRITERS
            .iter()
            .any(|package| self.world.holds_prefix(package));
        // Member references resolved before the JDK or the JARs arrived
        // answered nothing through their return types; the worker resolves
        // them again. The last message says the classpath has settled even
        // when it resends the same world, and the worker waits for that.
        if changed || settled.is_some() {
            self.queue(IndexJob::World(Arc::clone(&self.world), settled));
        }
        let persist = self.config.index.persist;
        if classpath.sources.is_none() || classpath.sources != self.jdk_sources_for {
            self.jdk_sources_for = classpath.sources.clone();
            self.jdk_sources = classpath.sources.and_then(|(src_zip, version)| {
                javelin_core::paths::cache_root(self.config.index.cache_dir.as_deref())
                    .inspect_err(
                        |err| tracing::debug!(%err, "jdk sources: no cache root, so no jump"),
                    )
                    .ok()
                    .map(|root| JdkSources::new(&root, src_zip, &version, persist))
            });
        }
        // ponytail: this skips `Workspace`, so `EngineState::supports` now
        // answers `true` for `References`, `WorkspaceSymbols` and
        // `Diagnostics` — none of which exists. Inert while no handler gates
        // on readiness, but one ordered ladder is now carrying two
        // independent facts: how much of the open file resolves, and whether
        // the classpath is in. The upgrade is to split them; do it when the
        // first handler needs to gate on one without claiming the other.
        // No JDK and no dependencies: the thread still says so, and that is
        // not the classpath arriving.
        if !self.world.is_empty() {
            self.engine.readiness = self.engine.readiness.max(Readiness::Dependencies);
        }
        self.judge_all();
    }

    /// The JDK's sources, for a handler that has a `src.zip` entry name and
    /// needs a file a client can open. `&mut` because extracting one is a
    /// write and because the archive is opened once and kept.
    pub(crate) fn jdk_sources(&mut self) -> Option<&mut JdkSources> {
        self.jdk_sources.as_mut()
    }

    /// A dependency's sources JAR, for a site the classpath grafted out of
    /// it — see `javelin_classpath::graft_sources`.
    pub(crate) fn jar_sources(&mut self, archive: &std::path::Path) -> Option<&mut JdkSources> {
        if !self.jar_sources.contains_key(archive) {
            let root = javelin_core::paths::cache_root(self.config.index.cache_dir.as_deref())
                .inspect_err(|err| tracing::debug!(%err, "jar sources: no cache root, so no jump"))
                .ok()?;
            let sources = JdkSources::for_sources_jar(
                &root,
                archive.to_path_buf(),
                self.config.index.persist,
            );
            self.jar_sources.insert(archive.to_path_buf(), sources);
        }
        self.jar_sources.get_mut(archive)
    }

    /// The editor opened a file.
    ///
    /// Parsing happens here, synchronously, because it is the cheapest possible
    /// path to the file being useful and the whole product promise rests on it
    /// not waiting for anything else. A thread hop would add scheduling latency
    /// to the one operation that has none to spare, and there is nothing to
    /// answer from in the meantime — unlike an edit, which always has the
    /// previous version to fall back on.
    pub fn did_open(&mut self, id: DocumentId, version: i32, text: String) {
        // A reopen of a URI something is still parsing: that answer describes a
        // different file now.
        self.abandon(&id);
        let document = self.documents.open(id.clone(), version, text);
        // An editor restoring a session reopens a buffer with unsaved text,
        // which is what the user reads and so what the index must say. Read
        // back on the loop: a file's bytes cost less than the parse below.
        if let Some(path) = self.admitted(id.as_str()) {
            let on_disk = std::fs::read(&path).map(javelin_index::decode_source);
            if on_disk.ok().as_deref() != Some(document.text()) {
                self.buffered.insert(path.clone());
                self.queue(IndexJob::Buffer(path, Arc::clone(&document)));
            }
        }
        let started = std::time::Instant::now();
        match self.parser.parse(&document) {
            Ok(tree) => {
                tracing::debug!(
                    uri = id.as_str(),
                    version,
                    recovered = tree.has_errors(),
                    duration_us = started.elapsed().as_micros() as u64,
                    "parsed document"
                );
                self.install_analysis(id.clone(), Analysis::new(document, tree));
            }
            // Never fatal: an unparseable buffer costs this file its syntax
            // features, not the session.
            Err(err) => {
                self.analyses.remove(&id);
                tracing::warn!(uri = id.as_str(), %err, "parse failed");
            }
        }
        self.judge(&id);
    }

    /// A session's re-index jobs, as a test sees them: the channel the worker
    /// would read, over `roots`, with no worker behind it.
    #[cfg(test)]
    pub(crate) fn watching(&mut self, roots: Vec<std::path::PathBuf>) -> Receiver<IndexJob> {
        let (jobs, queued) = crossbeam_channel::unbounded();
        self.workspace_jobs = Some(jobs);
        self.roots = roots;
        queued
    }

    /// The editor saved a file: what is on disk is now what the buffer says.
    pub fn did_save(&mut self, id: &DocumentId) {
        if let Some(path) = self.admitted(id.as_str()) {
            self.buffered.remove(&path);
            self.queue(IndexJob::Disk(path));
        }
    }

    /// Files changed on disk behind the editor's back — a checkout, a build,
    /// a file created or deleted — as the client's watcher reports them.
    ///
    /// `kind` is LSP's `FileChangeType`: `1` created, `2` changed, `3`
    /// deleted. A source file is re-read — or, if it is open, its buffer is
    /// re-indexed instead, because the buffer and not the disk is what the
    /// user is reading. Anything else created or deleted under a root may be
    /// a folder, which takes files with it that no event names, so it queues
    /// a re-walk.
    pub fn did_change_watched(&mut self, changes: &[(String, u32)]) {
        if self.workspace_jobs.is_none() {
            return;
        }
        // ponytail: the classpath and the build's hints are read once, so a
        // changed build file withdraws every judgement until the next
        // session. The upgrade is asking the build tool again.
        if !self.build_changed
            && changes.iter().any(|(uri, _)| {
                crate::workspace::path_of(uri).is_some_and(|path| {
                    self.roots.iter().any(|root| {
                        path.strip_prefix(root)
                            .is_ok_and(javelin_build::is_build_path)
                    })
                })
            })
        {
            tracing::info!("build: a build file changed; diagnostics withheld until a restart");
            self.build_changed = true;
            self.judge_all();
        }
        for (uri, kind) in changes {
            // A `lombok.config` decides what the walk believes Lombok
            // generates, which the survey records.
            //
            // ponytail: one above the roots is read at every walk and
            // watched by nobody, so a change to it waits for the next
            // re-walk. The upgrade is a watcher outside the workspace,
            // which LSP's relative patterns allow and no client here asks
            // for.
            if crate::workspace::path_of(uri)
                .is_some_and(|path| path.file_name().is_some_and(|name| name == "lombok.config"))
            {
                self.queue(IndexJob::Rescan);
                continue;
            }
            // A build wrote or removed a generated source: the walk records
            // their names, and a new one would otherwise be unknown until the
            // next re-walk. Ahead of `admitted`, which takes a processor's
            // output as a source, because the survey's names are the walk's.
            //
            // ponytail: a whole re-walk per worker pause while a build writes,
            // which on a large tree is seconds of stat calls on the worker.
            // The upgrade is adding the created file's stem to the survey and
            // republishing, since a stale name only silences.
            if matches!(kind, 1 | 3)
                && let Some(path) = crate::workspace::path_of(uri)
                && javelin_index::is_generated_source(&self.roots, &path)
            {
                self.queue(IndexJob::Rescan);
                continue;
            }
            if let Some(path) = self.admitted(uri) {
                match self.documents.get(&DocumentId::new(uri.as_str())) {
                    Some(document) => self.queue(IndexJob::Buffer(path, document)),
                    None => self.queue(IndexJob::Disk(path)),
                }
                continue;
            }
            let Some(path) = crate::workspace::path_of(uri) else {
                continue;
            };
            let is_source = path
                .extension()
                .is_some_and(|extension| extension == "java");
            if matches!(kind, 1 | 3)
                && !is_source
                && (javelin_index::within(&self.roots, &path)
                    || javelin_index::reaches_processor_output(&self.roots, &path))
            {
                self.queue(IndexJob::Rescan);
            }
        }
    }

    /// The file a URI names, if it is one the workspace walk would index.
    fn admitted(&self, uri: &str) -> Option<std::path::PathBuf> {
        self.workspace_jobs.as_ref()?;
        let path = crate::workspace::path_of(uri)?;
        (javelin_index::admits(&self.roots, &path) && !self.through_a_symlink(&path))
            .then_some(path)
    }

    /// Whether a directory between the root and `path` is a symlink — a place
    /// the walk never goes, so an event there would index a file under a
    /// second path and duplicate every type in it.
    fn through_a_symlink(&self, path: &std::path::Path) -> bool {
        let Some(root) = self.roots.iter().find(|root| path.starts_with(root)) else {
            return false;
        };
        path.ancestors()
            .skip(1)
            .take_while(|ancestor| *ancestor != root.as_path())
            .any(|ancestor| {
                std::fs::symlink_metadata(ancestor)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
            })
    }

    /// Hand a job to the worker. A worker that has gone — its walk failed, or
    /// the session is ending — costs the re-index and nothing else.
    fn queue(&mut self, job: IndexJob) {
        let Some(jobs) = &self.workspace_jobs else {
            return;
        };
        if jobs.send(job).is_ok() {
            self.index_behind += 1;
        } else {
            tracing::debug!("workspace: the indexing worker is gone, change not indexed");
        }
    }

    /// The editor changed a file.
    ///
    /// The document store is updated here — it is a string splice, and every
    /// later edit is expressed against it — and the reparse goes to the pool.
    /// Requests arriving before it lands are answered from the previous
    /// analysis, which is coherent, rather than made to wait.
    pub fn did_change(&mut self, id: &DocumentId, version: i32, changes: &[Change]) {
        let Some((document, edits)) = self.documents.change_reporting(id, version, changes) else {
            // A change for a document we never saw opened. Dropping it is
            // correct: the client will re-open before it asks about the file.
            tracing::debug!(uri = id.as_str(), "change for an unopened document");
            return;
        };
        // ponytail: an edit arriving before the previous reparse lands is
        // reparsed from the last *installed* tree, and `JavaParser::reparse`
        // falls back to a full parse when the edits do not describe it. Correct,
        // but it re-scans the file while the user types fast. The upgrade is to
        // accumulate edit batches per document against the installed version;
        // do it if a profile says the full parses cost more than the bookkeeping.
        let previous = self
            .analyses
            .get(id)
            .map(|analysis| (analysis.tree.clone(), edits));
        // The buffer is what the user is reading now, so it is what the index
        // should say about this file until it is saved or closed. The worker
        // coalesces a burst of these into one re-index of the latest text.
        if let Some(path) = self.admitted(id.as_str()) {
            self.buffered.insert(path.clone());
            self.queue(IndexJob::Buffer(path, Arc::clone(&document)));
        }
        self.dispatch(id, document, previous);
        self.judge(id);
    }

    pub fn did_close(&mut self, id: &DocumentId) {
        // Unsaved text must not outlive its buffer: the file goes back to what
        // is on disk. A buffer never edited, or saved since, is what the disk
        // already says, and closing it costs nothing.
        if let Some(path) = self.admitted(id.as_str())
            && self.buffered.remove(&path)
        {
            self.queue(IndexJob::Disk(path));
        }
        self.documents.close(id);
        self.analyses.remove(id);
        self.hint_memos.remove(id);
        self.hints_await_parse.remove(id);
        self.abandon(id);
        self.clear(id);
        if self.documents.is_empty() {
            // The classpath is not per-document: closing the last file loses
            // the readiness that a parsed file bought, never the JDK behind it.
            self.engine.readiness = if self.world.is_empty() {
                Readiness::Starting
            } else {
                Readiness::Dependencies
            };
        }
    }

    /// The current tree for a document, if it is open and parsed.
    ///
    /// May describe an older version than the document store holds while a
    /// reparse is in flight.
    pub fn tree(&self, id: &DocumentId) -> Option<&SyntaxTree> {
        self.analyses.get(id).map(|analysis| &analysis.tree)
    }

    /// What a document declares, built on demand and kept until it changes.
    ///
    /// Building costs a walk of the whole tree, and an editor asks for the
    /// outline far more often than the user types, so the answer for a version
    /// is computed once. Taking `&mut self` is what lets it be lazy: nothing
    /// pays for a view of a file no one asked about.
    pub fn syntax(&mut self, id: &DocumentId) -> Option<&FileSyntax> {
        // The store is the authority on what is open; asking it first is what
        // makes "not open" and "not parsed" the same answer to a caller.
        self.documents.get(id)?;
        self.analyses.get_mut(id)?.syntax()
    }

    /// Everything a query about one file needs, built on demand.
    ///
    /// One accessor rather than three, because a caller cannot hold the results
    /// of three `&mut self` calls at once — and because the three must describe
    /// the same version to be worth combining.
    pub fn view(&mut self, id: &DocumentId) -> Option<FileView<'_>> {
        let current = self.documents.get(id)?.version();
        let analysis = self.analyses.get_mut(id)?;
        if analysis.version() != current {
            tracing::debug!(
                uri = id.as_str(),
                answering = analysis.version(),
                buffer = current,
                "answering from the previous view while a reparse is in flight"
            );
        }
        analysis.view()
    }

    /// Install a finished reparse, unless the buffer has moved past it.
    ///
    /// The sequence number is the real guard: it matches only the dispatch that
    /// is still wanted, which covers a document closed and reopened at a version
    /// number the editor reused. The version comparison states the invariant it
    /// upholds, at the cost of one integer compare.
    pub(crate) fn install(&mut self, outcome: Outcome) {
        self.outstanding = self.outstanding.saturating_sub(1);
        if self.pending.get(&outcome.id).map(|p| p.seq) != Some(outcome.seq) {
            tracing::debug!(uri = outcome.id.as_str(), "discarding a superseded parse");
            return;
        }
        self.pending.remove(&outcome.id);

        // Cancelled, failed, or panicked. The previous analysis stays: it still
        // describes a version this file really had.
        let Some(analysis) = outcome.analysis else {
            return;
        };
        match self.documents.get(&outcome.id) {
            Some(document) if document.version() == analysis.version() => {
                self.install_analysis(outcome.id, analysis);
            }
            _ => tracing::debug!(
                uri = outcome.id.as_str(),
                version = analysis.version(),
                "discarding a parse of a version the buffer has left"
            ),
        }
    }

    /// The channel finished reparses arrive on, for the loop to select over.
    pub(crate) fn parse_results(&self) -> Receiver<Outcome> {
        self.pool.results().clone()
    }

    /// Block until every dispatched reparse has landed.
    ///
    /// Never called from the request path — the whole point of the pool is that
    /// the loop does not wait for it. This exists so tests and benchmarks can
    /// assert against, or measure, a settled server without sleeping and hoping.
    pub fn settle(&mut self) {
        while self.outstanding > 0 {
            let outcome = self.pool.results().recv_timeout(SETTLE_TIMEOUT);
            match outcome {
                Ok(outcome) => self.install(outcome),
                Err(_) => {
                    tracing::error!(
                        outstanding = self.outstanding,
                        "gave up waiting for the parse pool"
                    );
                    return;
                }
            }
        }
    }

    fn install_analysis(&mut self, id: DocumentId, analysis: Analysis) {
        // A parsed file is all local resolution needs: the scope tree is built
        // from this tree alone and never fails, so definition, hover and
        // completion within the file are live from here. Nothing below this
        // point waits on the workspace.
        self.engine.readiness = self.engine.readiness.max(Readiness::File);
        if self.hints_await_parse.remove(&id) {
            self.owe_hint_refresh();
        }
        self.analyses.insert(id, analysis);
    }

    fn dispatch(
        &mut self,
        id: &DocumentId,
        document: Arc<TextDocument>,
        previous: Option<(SyntaxTree, EditBatch)>,
    ) {
        self.abandon(id);
        self.next_seq += 1;
        let token = CancellationToken::new();
        self.pending.insert(
            id.clone(),
            Pending {
                seq: self.next_seq,
                token: token.clone(),
            },
        );
        self.outstanding += 1;
        self.pool.submit(Job {
            seq: self.next_seq,
            document,
            previous,
            token,
        });
    }

    /// Cancel whatever reparse is in flight for a document.
    ///
    /// Its outcome still arrives and is still counted; it is just discarded.
    fn abandon(&mut self, id: &DocumentId) {
        if let Some(pending) = self.pending.remove(id) {
            pending.token.cancel();
        }
    }
}

impl Drop for ServerState {
    /// Cancel before the pool is joined.
    ///
    /// Field drop order runs this first, so every queued job is already
    /// cancelled by the time `Pool::drop` waits for the workers: they see the
    /// flag, drop the job and move on instead of reparsing a file nobody will
    /// ever ask about again.
    fn drop(&mut self) {
        for (_, pending) in self.pending.drain() {
            pending.token.cancel();
        }
        // The classpath reader is never joined, so this is not a handshake:
        // it is what makes a reader still walking `src.zip` stop within one
        // zip entry if the process outlives the session.
        self.classpath_cancel.cancel();
        javelin_build::kill_child(&self.build_child);
        // The same for the workspace walk, which stops within one directory
        // and one batch of files rather than indexing a whole monorepo nobody
        // is waiting for.
        self.workspace_cancel.cancel();
    }
}

/// Translate an LSP content change into a document-store change.
pub fn to_change(event: &lsp_types::TextDocumentContentChangeEvent) -> Change {
    match event.range {
        None => Change::full(event.text.clone()),
        Some(range) => Change::incremental(
            Range::new(to_position(range.start), to_position(range.end)),
            event.text.clone(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use javelin_document::Position;
    use javelin_semantic::Externals as _;

    fn queued(jobs: &Receiver<IndexJob>) -> Vec<String> {
        jobs.try_iter()
            .map(|job| match job {
                IndexJob::Disk(path) => format!("disk {}", path.display()),
                IndexJob::Buffer(path, document) => {
                    format!("buffer {} {}", path.display(), document.text())
                }
                IndexJob::Rescan => "rescan".to_owned(),
                IndexJob::World(_, settled) => format!("world {settled:?}"),
            })
            .collect()
    }

    #[test]
    fn an_edit_queues_the_buffers_text_for_re_indexing() {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        let jobs = state.watching(vec![std::path::PathBuf::from("/w")]);
        let id = DocumentId::new("file:///w/src/A.java");
        state.did_open(id.clone(), 1, "class A {}".to_owned());
        // Nothing is on disk at `/w`, so the open queues the buffer as well.
        let _ = queued(&jobs);
        state.did_change(&id, 2, &[Change::full("class B {}")]);
        assert_eq!(queued(&jobs), vec!["buffer /w/src/A.java class B {}"]);
    }

    /// An editor restoring a session reopens a buffer with unsaved text,
    /// which the index must say until it is saved or closed; a buffer that
    /// is what the disk says costs nothing.
    #[test]
    fn opening_a_buffer_the_disk_disagrees_with_queues_its_text() {
        let root = std::env::temp_dir().join(format!("javelin-open-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).expect("dir");
        std::fs::write(root.join("src/A.java"), "class A {}").expect("file");
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        let jobs = state.watching(vec![root.clone()]);
        let uri = |name: &str| format!("file://{}/src/{name}", root.display());
        state.did_open(DocumentId::new(uri("A.java")), 1, "class A {}".to_owned());
        state.did_close(&DocumentId::new(uri("A.java")));
        assert_eq!(queued(&jobs), Vec::<String>::new(), "as on disk");
        assert_eq!(state.index_behind, 0);
        state.did_open(DocumentId::new(uri("A.java")), 1, "class B {}".to_owned());
        assert_eq!(
            queued(&jobs),
            vec![format!(
                "buffer {} class B {{}}",
                root.join("src/A.java").display()
            )]
        );
        assert_eq!(state.index_behind, 1);
        state.did_close(&DocumentId::new(uri("A.java")));
        assert_eq!(
            queued(&jobs),
            vec![format!("disk {}", root.join("src/A.java").display())],
            "and closing it goes back to the disk"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_save_a_close_and_a_watched_change_queue_a_read_from_disk() {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        let jobs = state.watching(vec![std::path::PathBuf::from("/w")]);
        let id = DocumentId::new("file:///w/src/A.java");
        state.did_open(id.clone(), 1, "class A {}".to_owned());
        // Nothing is on disk at `/w`, so the open queues the buffer as well.
        let _ = queued(&jobs);
        state.did_change(&id, 2, &[Change::full("class B {}")]);
        state.did_save(&id);
        state.did_change(&id, 3, &[Change::full("class C {}")]);
        state.did_close(&id);
        state.did_change_watched(&[("file:///w/src/Gone.java".to_owned(), 3)]);
        assert_eq!(
            queued(&jobs),
            vec![
                "buffer /w/src/A.java class B {}",
                "disk /w/src/A.java",
                "buffer /w/src/A.java class C {}",
                "disk /w/src/A.java",
                "disk /w/src/Gone.java",
            ]
        );
    }

    /// A build writing a generated source changes the names the walk
    /// records; changing one it already wrote does not, unless it is a
    /// processor's output, which the walk indexes like a source.
    #[test]
    fn a_generated_source_appearing_rewalks_the_workspace() {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        let jobs = state.watching(vec![std::path::PathBuf::from("/w")]);
        let generated = "file:///w/target/generated-sources/xjc/p/AType.java";
        state.did_change_watched(&[(generated.to_owned(), 2)]);
        assert!(queued(&jobs).is_empty());
        state.did_change_watched(&[(generated.to_owned(), 1)]);
        assert_eq!(queued(&jobs), vec!["rescan"]);
        let processed = "file:///w/target/generated-sources/annotations/p/ImmutableA.java";
        state.did_change_watched(&[(processed.to_owned(), 2)]);
        assert_eq!(
            queued(&jobs),
            vec!["disk /w/target/generated-sources/annotations/p/ImmutableA.java"]
        );
        state.did_change_watched(&[(processed.to_owned(), 3)]);
        assert_eq!(queued(&jobs), vec!["rescan"]);
        // A `clean` may be reported as the build directory alone.
        for gone in [
            "file:///w/m/build",
            "file:///w/target/generated-sources/annotations/p",
        ] {
            state.did_change_watched(&[(gone.to_owned(), 3)]);
            assert_eq!(queued(&jobs), vec!["rescan"], "{gone}");
        }
        state.did_change_watched(&[("file:///w/target/classes".to_owned(), 3)]);
        assert!(queued(&jobs).is_empty(), "no output lies there");
    }

    /// Judged only where the walk would index: a file outside the roots
    /// names types from a classpath the world does not describe. And never
    /// a processor's output, which the walk indexes for navigation alone.
    #[test]
    fn only_a_file_the_walk_would_index_is_judged() {
        let mut config = Config::default();
        config.diagnostics.debounce_ms = 1;
        let mut state = ServerState::new(config).expect("grammar loads");
        let _jobs = state.watching(vec![std::path::PathBuf::from("/w")]);
        state.install_classpath(Classpath::hand_built(&["java.lang.Object"]));
        state.dependency_report = Some(crate::classpath::DependencyReport {
            key: 0,
            complete: true,
            processors: false,
            loose: true,
            codegen: false,
            rewrites: false,
            release: None,
            modules: None,
        });
        state.survey = Some(Arc::new(javelin_index::Survey::default()));
        state.watching = true;
        let results = state.diagnostics_results();
        let text = "class A { Missing m; }";
        state.did_open(DocumentId::new("file:///elsewhere/A.java"), 1, text.into());
        state.did_open(DocumentId::new("file:///w/target/A.java"), 1, text.into());
        // Walked for navigation, and the build's to judge.
        let output = "file:///w/target/generated-sources/annotations/p/A.java";
        state.did_open(DocumentId::new(output), 1, text.into());
        // A hand-written package named like build output is judged.
        let package = "file:///w/src/main/java/build/generated/B.java";
        state.did_open(DocumentId::new(package), 1, text.into());
        state.did_open(DocumentId::new("file:///w/src/A.java"), 1, text.into());
        let mut judged: Vec<String> = (0..2)
            .map(|_| {
                results
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .expect("the files under the root are judged")
                    .uri
            })
            .collect();
        judged.sort();
        assert_eq!(judged, vec!["file:///w/src/A.java", package]);
        assert!(
            results
                .recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "nothing else is"
        );
    }

    /// A document closed and opened again starts its versions over: a
    /// judgement of the old text is not about the new one.
    #[test]
    fn a_judgement_of_a_closed_documents_text_is_not_current_after_it_reopens() {
        let mut state = state();
        let id = DocumentId::new("file:///A.java");
        state.did_open(id.clone(), 0, "class A { Old o; }".into());
        let judged = crate::diagnostics::Published {
            uri: id.as_str().to_owned(),
            document: state.documents.get(&id),
            found: Vec::new(),
        };
        assert!(state.is_current(&judged));
        state.did_close(&id);
        state.did_open(id.clone(), 0, "class A { New n; }".into());
        assert!(!state.is_current(&judged));
    }

    #[test]
    fn a_watched_change_to_an_open_buffer_keeps_the_buffer_as_the_authority() {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        let jobs = state.watching(vec![std::path::PathBuf::from("/w")]);
        let id = DocumentId::new("file:///w/src/A.java");
        state.did_open(id.clone(), 1, "class A {}".to_owned());
        // Nothing is on disk at `/w`, so the open queues the buffer as well.
        let _ = queued(&jobs);
        state.did_change_watched(&[("file:///w/src/A.java".to_owned(), 2)]);
        assert_eq!(queued(&jobs), vec!["buffer /w/src/A.java class A {}"]);
    }

    #[test]
    fn a_deleted_folder_under_a_root_queues_a_rescan() {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        let jobs = state.watching(vec![std::path::PathBuf::from("/w")]);
        state.did_change_watched(&[
            ("file:///w/src/com/example".to_owned(), 3),
            ("file:///w/target/classes".to_owned(), 3),
            ("file:///elsewhere/pkg".to_owned(), 3),
        ]);
        assert_eq!(queued(&jobs), vec!["rescan"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_file_under_a_symlinked_directory_queues_nothing() {
        // The walk follows no symlink; an event must not index what it would
        // not, under a second path that duplicates every type in it.
        let root =
            std::env::temp_dir().join(format!("javelin-state-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("real")).expect("dirs");
        std::fs::write(root.join("real/A.java"), "class A {}").expect("write");
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).expect("symlink");
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        let jobs = state.watching(vec![root.clone()]);
        state.did_change_watched(&[(format!("file://{}/link/A.java", root.display()), 2)]);
        let _ = std::fs::remove_dir_all(&root);
        assert!(queued(&jobs).is_empty());
    }

    #[test]
    fn a_file_outside_every_root_or_under_build_output_queues_nothing() {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        let jobs = state.watching(vec![std::path::PathBuf::from("/w")]);
        let outside = DocumentId::new("file:///elsewhere/A.java");
        state.did_open(outside.clone(), 1, "class A {}".to_owned());
        state.did_change(&outside, 2, &[Change::full("class B {}")]);
        state.did_save(&outside);
        state.did_change_watched(&[
            ("file:///w/target/classes/A.java".to_owned(), 2),
            ("file:///elsewhere/notes.txt".to_owned(), 3),
            ("untitled:Untitled-1".to_owned(), 1),
        ]);
        assert!(queued(&jobs).is_empty());
    }

    fn state() -> ServerState {
        ServerState::new(Config::default()).expect("grammar loads")
    }

    /// Rename the class in `class <from> { … }`, incrementally.
    fn rename(state: &mut ServerState, id: &DocumentId, version: i32, from: &str, to: &str) {
        let start = Position::new(0, 6);
        let end = Position::new(0, 6 + from.len() as u32);
        state.did_change(
            id,
            version,
            &[Change::incremental(Range::new(start, end), to)],
        );
    }

    /// `Readiness::File` — not `Syntax` — because a parse is the only input the
    /// scope tree has. Anything less would make definition and hover, which
    /// [`javelin_protocol::Feature::available_from`] puts at `File`, unreachable
    /// for a file that is already fully resolvable.
    #[test]
    fn opening_a_file_makes_local_resolution_available_immediately() {
        let mut state = state();
        assert_eq!(state.engine.readiness, Readiness::Starting);
        state.did_open(DocumentId::new("file:///A.java"), 1, "class A {}".into());
        assert_eq!(state.engine.readiness, Readiness::File);
        assert!(state.engine.supports(javelin_protocol::Feature::Definition));
    }

    #[test]
    fn an_unparseable_file_does_not_take_the_session_down() {
        let mut state = state();
        let id = DocumentId::new("file:///Broken.java");
        state.did_open(id.clone(), 1, "class {{{ ???".into());
        assert_eq!(state.documents.len(), 1);
        state.did_open(DocumentId::new("file:///Fine.java"), 1, "class B {}".into());
        assert_eq!(state.engine.readiness, Readiness::File);
    }

    #[test]
    fn incremental_changes_reach_the_document_store() {
        let mut state = state();
        let id = DocumentId::new("file:///A.java");
        state.did_open(id.clone(), 1, "class A {}".into());
        rename(&mut state, &id, 2, "A", "Renamed");
        let document = state.documents.get(&id).expect("still open");
        assert_eq!(document.text(), "class Renamed {}");
        assert_eq!(document.version(), 2);
    }

    #[test]
    fn an_edit_leaves_a_tree_describing_the_new_version() {
        let mut state = state();
        let id = DocumentId::new("file:///A.java");
        state.did_open(id.clone(), 1, "class A {}".into());
        assert_eq!(state.tree(&id).map(|t| t.version()), Some(1));

        rename(&mut state, &id, 2, "A", "Renamed");
        state.settle();
        let tree = state.tree(&id).expect("reparsed");
        assert_eq!(tree.version(), 2);
        assert!(tree.root().to_sexp().contains("class_declaration"));
        assert!(!tree.has_errors());
    }

    /// The parser-level equivalence test in `tests/integration` proves that a
    /// reparse matches a full parse; nothing there would notice the server
    /// handing the pool a tree the edits do not describe, because both paths
    /// produce the same answer. This is what pins that the reuse path runs.
    #[test]
    fn an_edit_on_a_settled_document_is_reparsed_from_the_installed_tree() {
        let mut state = state();
        let id = DocumentId::new("file:///A.java");
        state.did_open(id.clone(), 1, "class A {}".into());
        state.settle();

        rename(&mut state, &id, 2, "A", "Renamed");
        let outcome = state
            .pool
            .results()
            .recv_timeout(SETTLE_TIMEOUT)
            .expect("the reparse lands");
        assert!(outcome.incremental, "the pool fell back to a full parse");
        state.install(outcome);
        assert_eq!(state.tree(&id).map(|t| t.version()), Some(2));
    }

    #[test]
    fn closing_a_document_drops_its_tree() {
        let mut state = state();
        let id = DocumentId::new("file:///A.java");
        state.did_open(id.clone(), 1, "class A {}".into());
        state.did_close(&id);
        assert!(state.tree(&id).is_none());
    }

    #[test]
    fn a_change_for_an_unopened_document_is_ignored() {
        let mut state = state();
        state.did_change(
            &DocumentId::new("file:///Ghost.java"),
            2,
            &[Change::full("x")],
        );
        assert!(state.documents.is_empty());
    }

    /// The one place `Readiness::Dependencies` becomes true, and the point of
    /// the whole background job: a hand-built world, because a unit test that
    /// read a real JDK would depend on the machine it runs on.
    #[test]
    fn readiness_reaches_dependencies_when_a_world_arrives() {
        let mut state = state();
        state.did_open(DocumentId::new("file:///A.java"), 1, "class A {}".into());
        assert_eq!(state.engine.readiness, Readiness::File);
        assert!(!state.is_complete());

        state.install_classpath(Classpath::hand_built(&["java.lang.String"]));
        assert_eq!(state.engine.readiness, Readiness::Dependencies);
        assert!(state.is_complete());
        assert_eq!(
            state
                .externals()
                .is_subtype("java.lang.String", "java.lang.String"),
            Some(true),
            "the world the job sent is the one the handlers ask"
        );
    }

    /// The worker holds its first resolution pass back for the settled
    /// world, so the classpath thread's last message must reach it even when
    /// the dependencies added nothing and the world is the one already sent.
    #[test]
    fn the_settled_classpath_reaches_the_worker_even_when_its_world_is_unchanged() {
        let mut state = state();
        let jobs = state.watching(vec![std::path::PathBuf::from("/w")]);
        let jdk = Classpath::hand_built(&["java.lang.String"]);
        let world = Arc::clone(&jdk.world);
        state.install_classpath(jdk);
        assert_eq!(queued(&jobs), vec!["world None"]);
        let mut last = Classpath::hand_built(&[]);
        last.world = world;
        last.dependencies = Some(crate::classpath::DependencyReport {
            key: 9,
            complete: true,
            processors: false,
            loose: true,
            codegen: false,
            rewrites: false,
            release: None,
            modules: None,
        });
        state.install_classpath(last);
        assert_eq!(queued(&jobs), vec!["world Some(9)"]);
    }

    /// A world every gate passes: the JDK in, a loose project, the walk
    /// clean, the files watched.
    fn whole() -> ServerState {
        let mut state = state();
        state.install_classpath(Classpath::hand_built(&["java.lang.Object"]));
        state.jdk_sources_for = Some((std::path::PathBuf::from("src.zip"), "21.0.2".into()));
        state.dependency_report = Some(crate::classpath::DependencyReport {
            key: 0,
            complete: true,
            processors: false,
            loose: true,
            codegen: false,
            rewrites: false,
            release: None,
            modules: None,
        });
        state.survey = Some(Arc::new(javelin_index::Survey::default()));
        state.watching = true;
        state
    }

    /// The classpath and the build's hints were read at startup; a build
    /// file changed since — a checkout bumping a dependency — may name JARs
    /// the world does not hold, so nothing is judged until the next session.
    #[test]
    fn a_changed_build_file_withdraws_every_judgement() {
        for (uri, kind) in [
            ("file:///w/pom.xml", 2),
            ("file:///w/app/build.gradle.kts", 2),
            ("file:///w/gradle/libs.versions.toml", 2),
            ("file:///w/buildSrc/src/main/kotlin/Deps.kt", 2),
            ("file:///w/settings.gradle", 1),
        ] {
            let mut state = whole();
            let _jobs = state.watching(vec![std::path::PathBuf::from("/w")]);
            assert!(state.diagnosable().is_some(), "{uri}: the baseline");
            assert!(state.whole_dependencies().is_some(), "{uri}: the baseline");
            state.did_change_watched(&[(uri.to_owned(), kind)]);
            assert!(state.diagnosable().is_none(), "{uri}");
            assert!(state.whole_dependencies().is_none(), "{uri}");
        }
        // What a build writes, and what the key does not count: a source
        // file, build output, Gradle's own state, a downloaded wrapper.
        for uri in [
            "file:///w/src/A.java",
            "file:///w/buildSrc/build/classes/Deps.class",
            "file:///w/buildSrc/.gradle/file-system.probe",
            "file:///w/.mvn/wrapper/maven-wrapper.jar",
            "file:///w/target/it/simple/pom.xml",
            "file:///w/app/build/tmp/build.gradle",
        ] {
            let mut state = whole();
            let _jobs = state.watching(vec![std::path::PathBuf::from("/w")]);
            state.did_change_watched(&[(uri.to_owned(), 1)]);
            assert!(state.diagnosable().is_some(), "{uri} is no build file");
        }
    }

    /// A tool that adds members to declared types, and an index behind a
    /// buffer, each withhold member diagnostics and leave the type names
    /// judged.
    #[test]
    fn every_member_gate_withholds_the_member_half_on_its_own() {
        let members = |state: &ServerState| state.diagnosable().expect("types judged").members;
        assert!(members(&whole()), "the baseline judges members");

        let mut state = whole();
        if let Some(report) = state.dependency_report.as_mut() {
            report.rewrites = true;
        }
        assert!(!members(&state), "a build naming Manifold or AspectJ");

        // Lombok is answered per type, by what its annotations write.
        for (on_classpath, imported) in [
            ("lombok.Getter", "lombok.Data"),
            (
                "manifold.ext.rt.api.Extension",
                "manifold.ext.rt.api.Extension",
            ),
        ] {
            let lombok = on_classpath.starts_with("lombok");
            let mut state = whole();
            state.install_classpath(Classpath::hand_built(&["java.lang.Object", on_classpath]));
            assert_eq!(members(&state), lombok, "{on_classpath} on the classpath");

            let mut state = whole();
            let sources = crate::workspace::testing::indexed(
                if lombok {
                    "lombok-import"
                } else {
                    "manifold-import"
                },
                &[(
                    "p/A.java",
                    &format!("package p;\nimport {imported};\npublic class A {{}}\n"),
                )],
            );
            state.install_workspace(crate::workspace::Workspace {
                index: Arc::clone(&sources.index),
                elapsed: std::time::Duration::ZERO,
                refreshed: false,
                survey: None,
                absorbed: 0,
            });
            assert_eq!(
                members(&state),
                lombok,
                "a workspace file importing {imported}"
            );
        }

        let mut state = whole();
        let (jobs, _queued) = crossbeam_channel::unbounded();
        state.workspace_jobs = Some(jobs);
        state.queue(IndexJob::Rescan);
        state.queue(IndexJob::Rescan);
        assert!(!members(&state), "two jobs the index has not taken in");
        let publish = |state: &mut ServerState, absorbed| {
            state.install_workspace(crate::workspace::Workspace {
                index: Arc::new(WorkspaceIndex::default()),
                elapsed: std::time::Duration::ZERO,
                refreshed: true,
                survey: None,
                absorbed,
            });
        };
        publish(&mut state, 1);
        assert!(!members(&state), "one still outstanding");
        publish(&mut state, 1);
        assert!(members(&state), "level again");
    }

    /// Each thing that makes the world too thin to judge against refuses on
    /// its own: a diagnostic judged against it would call a real type unknown.
    #[test]
    fn every_world_gate_refuses_diagnostics_on_its_own() {
        use crate::classpath::DependencyReport;
        assert!(whole().diagnosable().is_some(), "the baseline is judged");
        let survey = |edit: fn(&mut javelin_index::Survey)| {
            let mut survey = javelin_index::Survey::default();
            edit(&mut survey);
            Some(Arc::new(survey))
        };
        let report = |state: &mut ServerState, edit: fn(&mut DependencyReport)| {
            if let Some(report) = state.dependency_report.as_mut() {
                edit(report);
            }
        };
        type Gate = Box<dyn Fn(&mut ServerState)>;
        let gates: Vec<(&str, Gate)> = vec![
            ("no watcher", Box::new(|state| state.watching = false)),
            (
                "a lowered size limit",
                Box::new(|state| state.config.index.max_file_size_bytes = 1024),
            ),
            (
                "incomplete dependencies",
                Box::new(move |state| report(state, |report| report.complete = false)),
            ),
            (
                "a package the walk prunes",
                Box::new(move |state| state.survey = survey(|survey| survey.hidden = true)),
            ),
            (
                "a source generator that has generated nothing",
                Box::new(move |state| report(state, |report| report.codegen = true)),
            ),
            (
                "a level newer than the JDK",
                Box::new(move |state| report(state, |report| report.release = Some((17, 25)))),
            ),
            (
                "Java 8 against a JDK without Java EE",
                Box::new(move |state| report(state, |report| report.release = Some((8, 8)))),
            ),
        ];
        for (gate, apply) in gates {
            let mut state = whole();
            apply(&mut state);
            assert!(state.diagnosable().is_none(), "{gate}");
        }
        let mut state = whole();
        report(&mut state, |report| report.release = Some((17, 21)));
        assert!(
            state.diagnosable().is_some(),
            "an older level on a newer JDK"
        );
        // A generator has run when the walk indexed its output or saw a
        // name it left; a processor's output says nothing about it.
        for (ran, edit) in [
            (
                true,
                (|survey| survey.generator_output = true) as fn(&mut javelin_index::Survey),
            ),
            (true, |survey| {
                survey.generated.insert("Order".into());
            }),
            (false, |_| {}),
        ] {
            let mut state = whole();
            report(&mut state, |report| report.codegen = true);
            state.survey = survey(edit);
            assert_eq!(state.diagnosable().is_some(), ran);
        }
    }

    #[test]
    fn a_jdk_version_is_read_as_its_feature_number() {
        let mut state = state();
        for (version, feature) in [("21.0.2", 21), ("1.8.0_392", 8), ("25", 25), ("17-ea", 17)] {
            state.jdk_sources_for = Some((std::path::PathBuf::new(), version.into()));
            assert_eq!(state.jdk_feature(), Some(feature), "{version}");
        }
    }

    /// Readiness is per-document below `Dependencies` and per-session at it:
    /// the JDK did not go anywhere when the user closed their last tab.
    #[test]
    fn closing_the_last_document_keeps_the_classpath_the_job_read() {
        let mut state = state();
        let id = DocumentId::new("file:///A.java");
        state.did_open(id.clone(), 1, "class A {}".into());
        state.install_classpath(Classpath::hand_built(&["java.lang.String"]));
        state.did_close(&id);
        assert_eq!(state.engine.readiness, Readiness::Dependencies);
    }

    #[test]
    fn closing_the_last_document_resets_readiness() {
        let mut state = state();
        let id = DocumentId::new("file:///A.java");
        state.did_open(id.clone(), 1, "class A {}".into());
        state.did_close(&id);
        assert!(state.documents.is_empty());
        assert_eq!(state.engine.readiness, Readiness::Starting);
    }

    #[test]
    fn the_syntax_view_of_an_unchanged_document_is_not_rebuilt() {
        let mut state = state();
        let id = DocumentId::new("file:///A.java");
        state.did_open(id.clone(), 1, "class A { int x; }".into());

        // The rebuild would allocate its vectors before the cached view is
        // dropped, so a reused buffer address can only mean a reused view.
        let first = state.syntax(&id).expect("a view").declarations.as_ptr();
        let second = state.syntax(&id).expect("a view");
        assert_eq!(second.version, 1);
        assert_eq!(second.declarations.as_ptr(), first);
    }

    #[test]
    fn an_edit_produces_a_syntax_view_of_the_text_that_replaced_it() {
        let mut state = state();
        let id = DocumentId::new("file:///A.java");
        state.did_open(id.clone(), 1, "class A { int x; }".into());
        assert_eq!(state.syntax(&id).expect("a view").declarations[0].name, "A");

        rename(&mut state, &id, 2, "A", "Renamed");
        state.settle();

        let view = state.syntax(&id).expect("a view");
        assert_eq!(view.version, 2);
        assert_eq!(view.declarations[0].name, "Renamed");
    }

    #[test]
    fn a_document_that_is_not_open_has_no_syntax_view() {
        let mut state = state();
        let id = DocumentId::new("file:///A.java");
        state.did_open(id.clone(), 1, "class A {}".into());
        assert!(state.syntax(&id).is_some());
        state.did_close(&id);
        assert!(state.syntax(&id).is_none());
    }

    #[test]
    fn an_edit_produces_a_scope_tree_of_the_text_that_replaced_it() {
        let mut state = state();
        let id = DocumentId::new("file:///A.java");
        state.did_open(id.clone(), 1, "class A { int x; }".into());
        assert!(
            state
                .view(&id)
                .expect("a view")
                .scopes
                .resolve(10, "x")
                .is_some()
        );

        let range = Range::new(Position::new(0, 14), Position::new(0, 15));
        state.did_change(&id, 2, &[Change::incremental(range, "renamed")]);
        state.settle();

        let view = state.view(&id).expect("a view");
        assert_eq!(view.scopes.version(), 2);
        assert_eq!(view.document.version(), 2);
        assert!(view.scopes.resolve(10, "renamed").is_some());
    }

    #[test]
    fn a_document_that_is_not_open_has_no_view() {
        let mut state = state();
        assert!(state.view(&DocumentId::new("file:///Ghost.java")).is_none());
    }

    /// The defect a worker pool makes reachable: with the tree, the syntax view
    /// and the scopes in separate maps, a reparse that had not landed left the
    /// next request rebuilding both views from the *new* text against the *old*
    /// tree — wrong ranges rather than an old answer. Nothing here is allowed to
    /// describe a version any of the others does not.
    #[test]
    fn a_view_taken_while_a_reparse_is_in_flight_describes_one_version_throughout() {
        let mut state = state();
        let id = DocumentId::new("file:///A.java");
        state.did_open(id.clone(), 1, "class A { int x; }".into());
        rename(&mut state, &id, 2, "A", "Renamed");

        // Nothing installs a result outside the loop and `settle`, so this is
        // the in-flight moment, deterministically.
        assert_eq!(state.documents.get(&id).expect("open").version(), 2);
        let view = state.view(&id).expect("the previous view");
        assert_eq!(view.document.version(), 1);
        assert_eq!(view.tree.version(), 1);
        assert_eq!(view.syntax.version, 1);
        assert_eq!(view.scopes.version(), 1);
        assert_eq!(view.syntax.declarations[0].name, "A");

        state.settle();
        assert_eq!(state.view(&id).expect("a view").document.version(), 2);
    }

    #[test]
    fn a_superseded_reparse_is_cancelled_rather_than_run_to_completion() {
        let mut state = state();
        let id = DocumentId::new("file:///A.java");
        state.did_open(id.clone(), 1, "class A {}".into());

        rename(&mut state, &id, 2, "A", "Two");
        let abandoned = state.pending[&id].token.clone();
        assert!(!abandoned.is_cancelled());

        rename(&mut state, &id, 3, "Two", "Three");
        assert!(abandoned.is_cancelled());
    }

    /// Hazard one: a result for a version the buffer has left must never land on
    /// top of a newer one. Driven by hand rather than by racing two edits, so it
    /// fails on the logic rather than on the scheduler's mood.
    #[test]
    fn a_superseded_result_never_replaces_the_view_the_buffer_moved_past() {
        let mut state = state();
        let id = DocumentId::new("file:///A.java");
        state.did_open(id.clone(), 1, "class A {}".into());

        rename(&mut state, &id, 2, "A", "Two");
        // Take version 2's answer off the channel without installing it, then
        // let the buffer move on before handing it over.
        let stale = state.pool.results().recv().expect("an outcome");
        assert_eq!(stale.analysis.as_ref().map(Analysis::version), Some(2));
        rename(&mut state, &id, 3, "Two", "Three");

        state.install(stale);
        assert_eq!(state.tree(&id).map(SyntaxTree::version), Some(1));

        state.settle();
        assert_eq!(state.tree(&id).map(SyntaxTree::version), Some(3));
        assert_eq!(
            state.syntax(&id).expect("a view").declarations[0].name,
            "Three"
        );
    }

    #[test]
    fn reopening_a_file_discards_the_reparse_of_the_one_it_replaced() {
        let mut state = state();
        let id = DocumentId::new("file:///A.java");
        state.did_open(id.clone(), 1, "class A {}".into());
        rename(&mut state, &id, 2, "A", "Two");

        // Same URI, same version number, different file. The in-flight answer
        // describes text this document no longer has.
        state.did_open(id.clone(), 2, "class Replacement {}".into());
        state.settle();
        assert_eq!(
            state.syntax(&id).expect("a view").declarations[0].name,
            "Replacement"
        );
    }

    #[test]
    fn every_dispatched_reparse_answers_exactly_once() {
        let mut state = state();
        let id = DocumentId::new("file:///A.java");
        state.did_open(id.clone(), 1, "class A {}".into());
        for version in 2..12 {
            rename(&mut state, &id, version, "A", "A");
        }
        state.settle();
        assert_eq!(state.outstanding, 0);
        assert!(state.pending.is_empty());
    }

    #[test]
    fn the_pool_is_sized_from_the_worker_configuration() {
        let mut config = Config::default();
        config.workers.background_threads = Some(2);
        let state = ServerState::new(config).expect("grammar loads");
        assert_eq!(state.pool.size(), 2);
    }

    #[test]
    fn a_full_content_change_event_becomes_a_full_replacement() {
        let event = lsp_types::TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: "class C {}".into(),
        };
        assert!(to_change(&event).range.is_none());
    }
}
