# CLAUDE.md

Context for AI assistants and human contributors working in this repository.
Read this before making changes.

## What this is

Javelin is a Java language server written in Rust, targeting Zed, VS Code and
any other LSP client. It exists because the established Java language servers
make you wait: the editor opens, and the file in front of you is inert text until
the workspace has been imported, the build system consulted, the JDK read, and
the world indexed.

Javelin's premise is that all of that work should happen *behind* the user rather
than in front of them.

```text
Editor starts
    ↓
Language server starts              < 50 ms
    ↓
Current file becomes useful         first parse, < 10 ms
    ↓
Syntax / outline
    ↓
Completion / navigation / diagnostics
    ↓
Workspace intelligence
    ↓
Full indexing / build-system integration   (background, never blocking)
```

Version 1 is a **reading** tool: reviewing Java, much of it machine-generated, at
the speed of scrolling. A reviewer's verbs are in — definition, references,
hover, outline, workspace symbol, implementations, type and call hierarchy,
highlight, semantic tokens, inlay hints, folding, type definition — and an
author's verbs are out: rename, code actions, auto-import, signature help,
formatting. Completion exists but is not being invested in.

The headline feature is a warning for a type, method or field name that resolves
to nothing, because generated code calls APIs that do not exist and a reader
cannot tell a hallucinated API from an unfamiliar one. It carries one constraint
harder than the feature: **a false positive costs more than ten missing
answers.** Compiler-grade diagnostics are permanently out — a type checker is the
second Java front end [ADR 0004](docs/adr/0004-classpath-source-of-truth.md)
exists to avoid. Javelin says a name is unknown; it never says a program is
wrong.

Where each feature stands, what it refuses on purpose and what is next:
[docs/roadmap.md](docs/roadmap.md). What has been measured:
[docs/performance.md](docs/performance.md). How a human checks it in Zed:
[docs/manual-testing-zed.md](docs/manual-testing-zed.md).

## The invariant

**Ambiguity answers nothing, never a guess.** Every path that *picks* a member
goes through `javelin_semantic::infer::sole_candidate`; `apply.rs` claims only
exclusion, never applicability, so a new fact may turn a refusal into an answer
and may never turn an honest refusal into a guess. A missing answer is a
ceiling; a wrong answer is a bug. When a change trades one for the other, it
trades towards missing.

Deliberate shortcuts with a known ceiling carry a `ponytail:` comment naming the
ceiling and the upgrade path, so they stay findable (`git grep ponytail:`).

## Architectural philosophy

**Editor-agnostic core.** `javelin-server` is the only crate that knows LSP
exists. Editor integrations are thin launchers with no Java logic, so Zed and
VS Code cannot drift apart.

**Progressive availability.** "Not ready yet" is a normal answer, not an error.
`javelin_protocol::Readiness` says how warm the engine is. Requests never wait
for a higher readiness level: with no JDK found, or the read still running, the
server holds a `ClasspathExternals` over an empty `TypeWorld`, which answers
"cannot say" to everything.

**Incremental by default.** Editing one file should invalidate the smallest
possible amount of state.

**Snapshots, not shared mutation.** `TextDocument` is immutable; an edit produces
a new one. Background work holding version 7 keeps a coherent view while the user
types version 9.

**Cancellation is first-class.** Every request gets a `CancellationToken`.
Editors cancel constantly, so abandoned work must actually stop.

**Persistent where it pays.** The JDK's facts are cached in SQLite keyed by JDK
identity, machine-wide, so one eleven-second read of `src.zip` warms every
project. A workspace's own index is keyed by its path. **Bump the schema version
whenever a rule that derives stored rows changes**, or every existing cache keeps
serving the old answer.

**Measured, not assumed.** The benchmark harness exists before the features it
measures.

## Technology

| Concern | Choice | Why |
|---|---|---|
| Language | Rust 2024, pinned 1.97.0 | |
| LSP | `lsp-server` + `lsp-types` | No async runtime on the startup path; we own the scheduling policy. [ADR 0001](docs/adr/0001-lsp-library.md) |
| Parsing | Tree-sitter + `tree-sitter-java` | Error recovery and incremental reparsing. [ADR 0003](docs/adr/0003-parser.md) |
| Persistence | SQLite via `rusqlite` (bundled) | Crash-safe, WAL. [ADR 0002](docs/adr/0002-persistence.md) |
| JDK type facts | the JDK's `lib/src.zip`, read with the same parser | No second Java front end. [ADR 0004](docs/adr/0004-classpath-source-of-truth.md) |
| Dependency facts | `.class` files in JARs; `-sources.jar` only for where names are written | [ADR 0005](docs/adr/0005-class-files-for-dependencies.md) |
| Logging | `tracing` + `tracing-subscriber` | Structured events, stderr only |
| Errors | `thiserror` in libraries, `anyhow` in the binary | |

## Repository structure

```text
crates/
  core/        config, errors, cancellation, telemetry, cache paths
  protocol/    the engine⇄LSP contract; no lsp-types, no editor knowledge
  document/    open documents, versions, edits, UTF-16 position mapping
  parser/      Tree-sitter Java; the only crate that knows the grammar exists
  semantic/    types, inference, member resolution; the `Externals` window
  index/       persistent cache; the JDK's type facts and the workspace's own
  classpath/   JDK discovery, src.zip and class-file facts, Lombok, TypeWorld
  build/       Maven and Gradle: classpath, modules, sources download
  server/      transport, lifecycle, routing, cancellation, capabilities, handlers
  cli/         the `javelin` binary
extensions/    thin Zed and VS Code launchers
tests/         integration tests and Java fixtures
benchmarks/    Criterion benchmarks and the shared harness
docs/          architecture, performance, development, roadmap, ADRs
scripts/       check.sh, bench.sh, bench-startup.sh
```

The crate graph is acyclic and `javelin-core` depends on nothing in the
workspace. `javelin-classpath` depends on `javelin-parser`, because reading
`src.zip` *is* parsing Java, and `javelin-index` depends on `javelin-classpath`,
because what it stores is `TypeFacts`. `javelin-semantic` does *not* depend on
`javelin-classpath`: it declares the `Externals` trait and `javelin-server`
implements it over `TypeWorld` and the workspace index, which keeps the engine
testable against a hand-written world.

Two arms in `crates/semantic/src/apply.rs` assume an external fact cannot lead
back into the open buffer. They stay sound because `TypeWorld::is_subtype`
answers from the platform's facts alone as `from` — a workspace or JAR type as
the argument excludes nothing. Re-read that argument before widening what
`is_subtype` answers.

## Rules

**Do not make startup depend on complete workspace analysis.** The editor must be
usable before indexing, before the build system is consulted, before the JDK is
read.

**Do not block an interactive LSP request on full indexing.** Return a partial
answer.

**Do not do file-sized work on the request loop.** Requests are answered on the
loop because they are bounded by the answer they produce. Parsing and view
building are bounded by the file, so they run on the parse pool in
`crates/server/src/pool.rs`. A view's four pieces — document snapshot, tree,
syntax view, scopes — all come from one version; handing out a mix is a wrong
answer, not an old one.

**Do not couple editor-specific behaviour into the Java core.** If it only makes
sense for one editor, it belongs in that editor's extension.

**Do not introduce global mutable state.** `ServerState` is created in `serve`
and threaded through.

**Do not panic on the request path.** A malformed Java file, a stale `pom.xml`, or
a client sending garbage must degrade one feature, never the process. Use
`javelin_core::Error` and its `Severity` split.

**Do not write to stdout from the server.** Stdout is the LSP transport. Use
`tracing`; it goes to stderr.

**Do not add a large dependency without accounting for its startup cost.** The
budget is 50 ms. Record non-obvious choices as an ADR.

**Do not optimise on intuition when a benchmark can answer the question.**

**Do not claim functionality that does not exist.** No stub returning fake
results. `MethodNotFound` is the honest answer to an unimplemented request.

**Running the build tool runs the project's code**, so it stays behind
`build.importOnStartup`, off by default.

## Performance requirements

Interactive operations are judged at **p95**, not the mean.

| Operation | Target |
|---|---|
| Process startup | < 50 ms |
| `initialize` | < 100 ms |
| First parse | < 10 ms |
| Local completion | < 50 ms |
| Definition | < 50 ms |
| Local references | < 100 ms |
| Warm startup | hundreds of ms |

`scripts/bench.sh` for the Criterion suite, `scripts/bench-startup.sh` for
process startup. The known misses are all on generated god-classes (first parse
and completion at the largest tiers) and are recorded, with their causes, in
[docs/performance.md](docs/performance.md).

## Testing expectations

- New functionality comes with tests. Performance-sensitive functionality comes
  with a benchmark.
- Unit tests live in the crate they test. Cross-crate and LSP-level tests live in
  `tests/integration/`. Tests that need the real binary and real pipes live in
  `crates/cli/tests/` — the in-memory connection has no I/O threads, so a
  shutdown deadlock is invisible to every in-process test. Java fixtures live in
  `tests/fixtures/`.
- A test that reads the machine's own JDK is `#[ignore]`d with a reason and run
  by hand.
- Name tests after the behaviour that breaks:
  `positions_past_the_end_clamp_instead_of_panicking`, not `test_offset_2`.
- `scripts/check.sh` must pass. It is CI's steps in CI's order, plus recording
  the editor-pass numbers.
- **The editor pass is automated.** `crates/cli/tests/editor_pass.rs` drives the
  real binary over real pipes with a client that obeys the handshake, and
  `scripts/check.sh` regenerates
  [docs/editor-pass-results.md](docs/editor-pass-results.md) from it. It does
  not prove Zed's own rendering; a human walk in Zed does, and is described in
  [docs/manual-testing-zed.md](docs/manual-testing-zed.md).
- **A change that implements a new LSP request must extend `SURFACE` in
  `crates/cli/tests/reachability.rs`**, which asserts that every method the
  router answers is advertised and the reverse. Forgetting to advertise a
  request once shipped a feature that no editor could reach while every
  in-process test passed.

## Conventions

- 100-column lines, `rustfmt.toml`, Rust 2024.
- `unsafe` is denied workspace-wide.
- Comments explain *why*; the code says what. Do not narrate.
- Deliberate shortcuts with a known ceiling get a `ponytail:` comment naming the
  ceiling and the upgrade path.
