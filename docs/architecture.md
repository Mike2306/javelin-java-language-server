# Architecture

## The constraint everything follows from

A Java language server is expected to understand a workspace that takes a build
tool thirty seconds to describe and a JDK that takes several seconds to read.
The conventional answer is to do that work first and become useful afterwards.
Javelin inverts it: the work still happens, but nothing the user does waits for
it.

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

Concretely this means a request handler may never block on workspace indexing, a
Maven or Gradle refresh, dependency scanning, or any global analysis. It answers
with what is known now and the answer improves as the engine warms up. The
`javelin-protocol` crate encodes that as a first-class idea: `Readiness` says how
warm the engine is, and `Answer` gives a handler the vocabulary to return
`Partial` instead of waiting. Half of that is load-bearing:
`Readiness::Dependencies` is reached, observable and logged when the classpath
arrives. `Answer` is not — definition, hover and completion all construct one
through `handlers::answer`, and nothing reads it on the wire, because neither a
definition nor a hover response has anywhere to put "this could improve".

Completion is where a completeness claim does reach a client, and since Phase
3b's external-members slice `isIncomplete` is a conjunction of five proofs
rather than an unconditional `true`: the classpath has arrived, the answer came
from the member query rather than the prefix one, the receiver's type resolved,
the member walk reports it reached everything the type names, and the list was
not cut off at the candidate cap. Anything unproven leaves the flag `true`,
which costs a re-ask and never a wrong answer. In practice `false` is reached
for a receiver typed from the classpath and almost nowhere else — a file-local
receiver's list is genuinely short by `java.lang.Object`'s members, since
nobody writes that `extends` and the member walk follows written supertypes
only.

## Layers

```text
Editor (Zed, VS Code, any LSP client)
   │
   ▼
LSP layer                javelin-server
   │
   ▼
Request / task scheduler javelin-server  (single-threaded loop + parse pool)
   │
   ├── Document engine   javelin-document
   ├── Parser            javelin-parser
   ├── Semantic engine   javelin-semantic
   ├── Symbol / index    javelin-index        (type facts; no references)
   ├── Class / JAR / JDK javelin-classpath    (JDK sources only)
   ├── Build systems     javelin-build
   └── Persistent cache  javelin-index
                              │
                              ▼
                         javelin-core (config, errors, cancellation, telemetry)
```

The graph is acyclic and `javelin-core` depends on nothing in the workspace,
but it is not one chain: `javelin-classpath` sits above `javelin-parser` and
`javelin-document`, because reading a JDK's `src.zip` is parsing Java, and
`javelin-index` sits above `javelin-classpath`, because what it stores is
`TypeFacts`. `javelin-semantic` depends on neither — it declares the
`Externals` trait and `javelin-server` implements it over the classpath *and*
the workspace source index, which is how a name in the open buffer comes to
mean a sibling file without the engine reaching into one.
`javelin-server` is still the only crate that knows LSP exists, and nothing
below it knows what an editor is.

## What each crate owns

| Crate | Owns | State |
|---|---|---|
| `javelin-core` | Config, error taxonomy, cancellation, telemetry, cache paths | Implemented |
| `javelin-protocol` | The engine⇄LSP contract: `Readiness`, `Feature`, `Answer` | Implemented |
| `javelin-document` | Open documents, versions, edits, UTF-16 position mapping | Implemented |
| `javelin-parser` | Tree-sitter Java parsing | Full and incremental; `FileSyntax`, `ScopeTree` |
| `javelin-semantic` | Types, scopes, resolution, diagnostics | Resolution; no diagnostics |
| `javelin-index` | Persistent cache; later symbol/reference indexes | Cache, JDK type facts, workspace source index |
| `javelin-classpath` | JDK discovery, JAR and `.class` metadata | `src.zip` facts only |
| `javelin-build` | Build-system abstraction, Maven and Gradle | Detection only |
| `javelin-server` | Transport, lifecycle, routing, the pool | Lifecycle, sync, nine features |
| `javelin` (`crates/cli`) | `serve`, `doctor`, `parse` | Implemented |

The nine features are `documentSymbol`, `definition`, `hover`, `completion`,
`workspace/symbol`, `foldingRange`, `selectionRange`, `documentHighlight` and
`semanticTokens/full`. The first four are answered from the open buffer's
`FileSyntax` and `ScopeTree` — by
`javelin-parser`'s offset queries, and, for a member selection or a call, by
`javelin-semantic`'s type model over the same two views. The last four, added
in Phase 5a, read only the same two views and nothing else — no classpath, no
workspace index — through `javelin-parser`'s own `folding_ranges`,
`selection_ranges`, `occurrences_at` and `semantic_tokens`. The classpath adds
what a *name* outside the file means and, since Phase 3b's external-members
slice, the members that name has: a `.` on a receiver typed from the classpath
is answered from `TypeFacts` rather than from any file, and `members_of` merges
that list with the file-local walk's. Since the definition slice a fact also
says where it was written down, so a jump into a classpath member extracts that
one `src.zip` entry under the cache root and the location names the extracted
file. And since Phase 3b's fifth slice the workspace source index is read on the
request path too, by two features that read it in opposite ways.
`workspace/symbol` *lists* what is in it — no document is opened, no name is
resolved, the classpath is not asked. Definition and hover *resolve* against
it: since the sixth slice `ClasspathExternals` answers `Externals::resolve`
out of one table over the workspace's own types and the JDK's, so a name the
open buffer does not declare can mean a sibling source file, and the server
turns the fully-qualified name back into a location out of the index. Only
`resolve` reads the workspace half — `is_subtype` and `members` stay the JDK's
alone, because `javelin-semantic`'s overload elimination excludes on a world
merely having heard of a type and a fact about a file the user can edit must
not license that. All three of those answers describe what was on *disk* at the
last walk rather than what the user is looking at.

### Two deliberate merges

The scaffolding brief listed `classfile` and `jdk` as separate crates, and
`maven` and `gradle` as separate from `build`. They are merged, because four
empty crates are four `Cargo.toml` files and no boundary. `javelin-classpath`
holds JDK discovery today and will hold class-file reading; `javelin-build`
will hold the
Maven and Gradle backends as modules. Splitting them later is a `git mv` and a
manifest, and by then the boundary will be informed by real code.

## State and concurrency

Four rules keep the concurrency story simple enough to reason about:

**Snapshots, not shared mutation.** `TextDocument` is immutable. An edit produces
a new one. A background task holding version 7 keeps a coherent view while the
user is typing version 9, with no lock and no torn reads.

**Cancellation is cooperative and pervasive.** Every request gets a
`CancellationToken` from `InFlight`, and every handler polls it. Editors cancel
constantly — each keystroke invalidates the previous completion request — so
work nobody is waiting for must actually stop, not run to completion and have
its result discarded.

Where that bites today is the pool: a superseded reparse sees a cancelled token
and drops the job. A request handler's token, in contrast, can only already be
cancelled when it is called, because the loop processes one message at a time
and a `$/cancelRequest` cannot overtake the request it cancels. The polling is
there so that stops being true the day a request moves off the loop, and so the
handlers are testable against a cancelled token now.

**No global mutable state.** `ServerState` is created in `serve` and threaded
through. Tests spin up independent servers with nothing leaking between them.

**Expensive work runs on a pool; the loop stays single-threaded.** Requests are
answered on the loop, because they read cached state and are bounded by the
answer. Reparsing is bounded by the *file*, so `didChange` updates the document
store on the loop — a string splice — and hands the parse and both view builds
to the pool in `crates/server/src/pool.rs`, sized from
`workers.backgroundThreads` and floored at one worker — zero is not "less
background work", it is no reparsing at all for the rest of the session. The
loop then selects between the client and the pool. `didOpen` still parses
inline: it is the "file becomes useful" path, there is no previous view to
answer from, and a thread hop is latency it has nowhere to hide.

Three rules follow from that, and they are the whole correctness story:

- **A result is installed only if it is still wanted.** Each dispatch carries a
  sequence number; an outcome whose number is not the one the loop last asked
  for is dropped, as is one whose version the document store has moved past. A
  stale tree must never land on top of a newer one.
- **A superseded reparse is cancelled.** Dispatching for a document cancels the
  reparse already in flight for it, so a fast typist's intermediate versions
  stop rather than finish into a channel nobody reads. Every job answers exactly
  once either way, so nothing counting outstanding work can get stuck.
- **Requests never wait for the pool.** Until a result lands,
  `ServerState::view` answers from the previous `Analysis` — one struct with the
  document snapshot, its tree, its syntax view and its scopes, so the four
  cannot disagree. A mix of versions would be a wrong answer, which is the one
  thing not allowed. An older-but-coherent answer is an ordinary result and is
  not marked as anything: `Answer`'s `Partial` turns on classpath readiness and
  on whether anything was found, never on which version the view came from, so
  a fresh view can answer `Partial` and a stale one `Complete`.

Shutdown closes the job queue before joining the workers, for the same reason
`run_stdio` drops the connection before joining the I/O threads: a held sender
is a deadlock. Cancelling the live reparses first is what makes the join quick
rather than merely finite.

## Errors

`javelin_core::Error` sorts failures by how loud they should be, not by where
they came from:

- `Degrade` — parse failures, cancellations, build-system errors. Log at debug,
  keep serving, return a less complete answer. A malformed Java file is the
  normal state of a file being typed.
- `Report` — configuration and I/O problems. Worth telling the user once.
- `Bug` — `Error::Internal` only. An invariant inside Javelin was violated.

Nothing in this list is allowed to take the process down.

## Position encoding

LSP counts characters in UTF-16 code units; Rust strings are UTF-8. The
conversion lives in exactly one place, `javelin_document::LineIndex`, and the
server advertises `utf-16` to match. Out-of-range positions clamp rather than
error: an editor can legitimately send a position from a revision the server has
already replaced.

## Further reading

- [`docs/performance.md`](performance.md) — budgets and how they are measured
- [`docs/development.md`](development.md) — building, testing, benchmarking
- [`docs/roadmap.md`](roadmap.md) — phases
- [`docs/adr/`](adr/) — why the major dependencies were chosen
