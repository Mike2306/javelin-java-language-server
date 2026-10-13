# Roadmap

Each phase has a goal stated as an experience, not a feature list. The features
are how the goal gets met; the goal is what decides whether the phase is done.

## Version 1 — what it is for

Decided 2026-09-21, and it reorders everything from Phase 3b's remaining
slices onward. Nothing already signed off changes.

Javelin's version 1 is a **reading** tool. The use it is built for is reviewing
Java — much of it machine-generated — at the speed of scrolling, and without
waiting for a workspace import to finish first. That is a different product
from an IDE back end, and the difference is not polish. It is which verbs are
in and which are out.

**In, because a reviewer uses them:** definition, references, hover, outline,
workspace symbol, implementations, type and call hierarchy, document
highlight, semantic tokens, inlay hints, folding. **Out of version 1, because
they are an author's verbs:** completion beyond what already exists,
auto-import, code actions, signature help, rename, formatting. Completion is
not removed and not un-advertised — it is built, it works, and a reviewer who
types does not deserve a regression. It stops being *invested in*, which is
what the owed completion upgrades under Phase 5b below now mean: recorded, not scheduled.

**The headline feature is unresolved-symbol diagnostics**, which is Phase 3c,
and it is the one place version 1 answers a question rather than a navigation.
Generated Java calls methods that do not exist, on types that do not have
them, from packages nothing imported; a reader cannot tell a hallucinated API
from an unfamiliar one by looking at it. A server that can say *this name
resolves to nothing, and the index is complete enough for that to mean
something* is most of why reading with it beats reading the file twice.

It carries one constraint, and it is harder than the feature: **a false
positive costs more than ten missing answers.** Marking a real symbol unknown
teaches the reviewer to stop reading the squiggles, and a feature nobody reads
is worse than one that is absent, because it was paid for. The invariant the
engine already runs on — ambiguity answers nothing, never a guess — is that
constraint stated one layer down, which is why 3c is reachable from here
rather than a rewrite.

**Latency is the premise rather than a target.** Every phase below adds work
that resolves names, and resolving names is the work that gets slow. The rule
that keeps it honest is the one Phase 3a already follows twice: anything that
reads a build system, an archive or a tree of files runs on a background
thread, is cached on disk, and upgrades an answer instead of gating one. A
version 1 that is complete and takes ten seconds to become useful is a
version 1 that failed, because jdtls already does that.

### Three tracks

They are listed as tracks rather than as phases because none of them blocks
another, and the phase numbers they map onto are not in execution order any
more.

**Track A — make names resolve.** 3b's remaining member slice, then Phase 4's
dependency classpath, then Phase 3c. This is the long one and it is what the
headline feature stands on: a diagnostic cannot be sure a name is unknown
until everything a file can name is in the index, and in a real project most
of what a file names came from a JAR.

**Track B — the reviewer's verbs, workspace-only.** References, which is the
most-used verb on the list and needs neither a compiler nor a dependency
classpath — it is an occurrence table written by the walk Phase 3b already
runs. Then implementations and the type and call hierarchies, which are the
same index read through the inverse supertype edge. Then re-indexing on save
and watching the folder, which is the staleness ceiling both current index
readers carry: a jump that lands on the line a declaration *used* to be on is
a wrong answer, and a reviewer meets it constantly because they are reading
files they never opened.

**Track C — reading aids that need no index at all.** Phase 5a below, and it
has shipped: document highlight, semantic tokens, folding ranges and
selection ranges all fall out of the `FileSyntax` and `ScopeTree` that exist
today. They were the cheapest work in this document and they are most of what
makes reading a file *feel* fast, which is the product. Parameter inlay hints
turned out not to belong to this track — they need a parameter name on
`MemberFacts`, a fact `javelin-classpath`'s JDK reader does not record — so
they wait on that instead and stayed out of the phase.

### Order

5a, then Track B's references — both shipped — then 3b's members, then Phase 4, then Phase 3c,
then the hierarchies, then watching. It ships something visible in the first
week, puts the reviewer's most-used verb in before the multi-week chunk
starts, and leaves that chunk as the thing that unlocks the headline rather
than as the thing standing between the user and anything at all.

### The first release — decided 2026-10-01

Every reviewer verb exists except inlay hints. What the first usable release
for reviewing Java in Zed needs is *coverage* — common shapes where
definition, hover and references answered nothing — and a way to install it.
Four slices, in this order, each the smallest change that makes the common
case work:

1. **Type arguments on JDK and JAR members ← landed 2026-10-01.**
   `List<String>.get` answers `String`. Facts record type parameter names,
   supertype arguments and generic member types (schema version 15, one JDK
   re-read); the server's closure expresses inherited members in the asked
   type's own variables, and the engine binds them to the receiver's
   arguments. A method's own type variable, a raw receiver, a wildcard and a
   workspace type's parameters stay unknown — never `Object`. An external
   supertype's own override now hides what it inherits within one closure, so
   `class Names extends ArrayList<String>`'s `get` resolves. 
2. **Implicit lambda parameters ← landed 2026-10-01.**
   `list.forEach(x -> x.foo())` types `x` when the receiver's argument is
   known and the call's sole candidate takes a functional interface there
   whose single abstract method takes it; `? super T` is `T`. Abstractness is
   a new fact (schema 16). A lambda's return type stays unknown, a lambda
   outside a call's arguments is not typed, and the member diagnostic does not
   judge a lambda parameter's calls. 
3. **Override-aware references ← landed 2026-10-01**, through
   `hierarchy.rs`'s override edge read both ways
   (`hierarchy::override_line`), so references and implementation cannot
   disagree about what an override is. Not siblings; the call hierarchy's
   incoming calls stay exact. 
4. **Ship it ← landed 2026-10-01.** `extensions/zed` installs as a dev
   extension, finds `javelin` by setting or on `PATH` and passes it
   `initialization_options`; the README has a quick start; a release build
   starts in p95 9.8 ms; and the walk a human owes — every feature against a
   real Maven project with `build.importOnStartup` on — is in
   [`manual-testing-zed.md`](manual-testing-zed.md). 

Deferred until after the release: `-sources.jar`, `inlayHint`, multi-module
source roots, a span for external type names, qualified names,
most-specific-method selection, lambda return types, Lombok-generated
members.

### What this gives up, on purpose

Parity with jdtls as an *editing* back end, which is most of jdtls. Someone
who writes Java all day in Javelin will miss auto-import first and code
actions second, and both answers are "not in version 1" rather than "soon".

Formatting is not planned at any point. No Java formatter worth having exists
in Rust, writing one is a project rather than a phase, and the escape hatch if
it is ever demanded is shelling out to `google-java-format` — which is a
build-system-shaped dependency on a JVM being present, and therefore exactly
the kind of thing the startup budget exists to refuse.

And **compiler-grade diagnostics are out**, permanently, not deferred.
`incompatible types`, definite-assignment, exhaustiveness and the rest need a
type checker, and a type checker is a second Java front end — the thing
[ADR 0004](adr/0004-classpath-source-of-truth.md) exists to avoid. Version 1
says a name is unknown. It never says a program is wrong.

### Tier 2 — decided 2026-10-07

Tier 1 left every reviewer verb answering on the common shapes, and left the
headline feature silent on the project it was walked against — a private
Spring Boot codebase of about 6 700 files, called "the sample" below — which
has Lombok in 3 125 files, so the member half of the diagnostic has never
judged a call there. Tier 2 was chosen by measuring that project rather than
by the list of `ponytail:` ceilings, with a throwaway probe build that logs
why each refusal refused.

**What the measurement found.** Over 60 files sampled across its modules,
definition was asked at 6 734 tokens and answered 4 207, with no mismatch. Of
the 2 527 it did not answer, 1 125 are keywords and about 840 are words inside
string literals and SQL text blocks, so roughly 560 are real names. Those
split, by the first link that typed as unknown: annotation element names —
`value =` in `@GetMapping(value = …)` — about 130; statically imported
constants and methods about 115; the `LOG` field `@Slf4j` generates, renamed
by the project's `lombok.config`, and its calls about 90; a workspace
member's type arguments, kept unresolved since Tier 1's slice 8, so
`repository.findById(id).get()` stops at `Optional<?>`, and generic workspace
types, whose parameter names are not indexed, about 100 between them; a class
literal argument (`handle.attach(Repo.class)`) and a for-each `var` about 30;
record accessors about 10.

The member diagnostic, with the Lombok gate forced open in the probe build,
reached 7 932 member uses over the first 300 files and judged 3 381 of them —
43%, where today's build judges none — finding 25 names absent, every one
silenced by an existing gate (24 statically imported, one a possible nested
type), so no false positive. Of the uses it could not judge, 2 703 have a
receiver it cannot be certain of, 1 726 of them a chain hop, and 1 330 a
receiver whose list is not whole: 742 because seven of the workspace's own
module JARs (`modules/apps/*/build/libs/*-plain.jar`) are on another module's
Gradle classpath, which makes every type in those modules "a workspace copy of
a JAR type"; 377 enums and 70 records, whose implicit members nobody writes;
and 237 interfaces whose file has a static import, which the rule refusing a
supertype read through the tiers counts whatever the import's name.

**Nine slices, in this order.**

1. **The workspace's own build output is not a dependency** — **landed**,
   schema version 26. A JAR under a workspace root, below a `target` or
   `build` directory, is that workspace's compiled sources: its facts are
   still read, since a generator's output may be in them and nowhere else,
   but a workspace type only such JARs also hold is no longer a rival copy
   of itself, where the class file names nothing its source does not —
   Lombok's `canEqual` is generated so `@Data` passes. And a single static import refuses a supertype read through
   the tiers only when its name is the supertype's first segment.
2. **Members judged under Lombok** — **landed**, schema version 27 — where
   every Lombok annotation on a receiver's workspace frames is one
   `lombok_members` models or one that adds no name, and no frame sits below
   a renaming `lombok.config`. A file using `@ExtensionMethod` has no member
   judged, and a JDK or JAR receiver is judged wherever the file does not.
3. **Implicit enum and record members** — **landed**, schema version 28: `values()`, `valueOf(String)` and a
   record's accessors are members, so an enum's and a record's lists can be
   whole and `entry.date()` jumps.
4. **Static imports navigated** — **landed**, schema version 29: a statically imported field, enum constant or
   method jumps, hovers and types its chain.
5. **Workspace generics** — **landed**, schema version 30: a workspace type's and method's type-parameter
   names are indexed and a member's type arguments resolved, so
   `Optional<Entity>` from a workspace repository keeps its `Entity`, a
   generic workspace supertype substitutes, and a generic workspace method
   infers.
6. **`@Slf4j` and the other log annotations** — **landed**, schema version 31: they generate their field, named by
   `lombok.log.fieldName` where a `lombok.config` sets it.
7. **Certain chains through workspace members** — **landed**, no schema
   change — for the member diagnostic: a hop through a workspace getter is certain where its written type cannot
   be shadowed in its own file.
8. **Class literals and for-each `var`** — **landed**, schema version 32: `Foo.class` is a `Class<Foo>`, and
   `for (var x : xs)` is typed from the iterable's or the array's element.
9. **Annotation element names** — **landed**, schema version 33: an element-value pair's key
   jumps and hovers to the element's declaration, and is no longer read as a field's use.

**What the measurement dropped.** The shared accessibility check fixes a wrong
answer in *completion*, which version 1 does not invest in. The implicit
`Object` in the file-local walk cost five judgements in 7 932. No unqualified
call inside an anonymous class, no `@Delegate` and no protobuf, OpenAPI or
jOOQ output occurred in the sample. `@SuperBuilder` (13 files) and `@Singular`
(7) are rare next to `@Slf4j` (794). Method references, subtype unification
and target typing (`stream.collect(Collectors.toList())`) are each under
twenty tokens and are the most expensive to get right; constructor
resolution answers a `new`, which the sample shows readers do not ask at.
Each stays recorded where its `ponytail:` is.

### Tier 3 — decided 2026-10-08

Tier 2 left definition answering 1 140 of 1 604 tokens on the sample with no
mismatch and the member diagnostic judging under Lombok. What a reviewer still
cannot do is *read* a file without asking: a `var`, a lambda parameter and a
chain carry types nobody wrote down; a variable's type is one hop away and
there is no verb for the hop; `new Greeter(a, b)` answers nothing; and the
largest generated files — the ones this tool is for — get no colouring at all.
Tier 3 is six reading features, in this order:

1. **Type inlay hints.** `: Order` after a `var` local, after an implicit
   lambda parameter, and at a multi-line chain's line ends where the type
   changes. First because it is the widest gain for the least new machinery:
   `type_of` already answers and `inlay_hint.rs` already ships, so the work is
   the gates — a hint is a claim on every line in view, held to the
   parameter hints' bar or harder. **Landed**: typed through `type_of` in a
   strict mode that refuses any pick from a list not shown whole.
2. **Go to type definition.** `textDocument/typeDefinition` — `type_of` plus
   the placing definition already does. Second because it is the same source
   as slice 1 and nearly no new code: a new verb rather than a new answer.
   **Landed**: the type a hint would show, raw types admitted, placed as
   definition places a type name.
3. **Constructors as targets.** `new Greeter(a, b)` jumps to the constructor
   its arguments select, references on a constructor list its `new` calls,
   and a constructor call gets parameter-name hints. Third because it is the
   first slice that changes what is stored, and it builds on slice 1's hint
   handler. **Landed**: selected among the type's own constructors where the
   list is whole, under the hints' gates; schema version 34.
4. **Generator output beyond annotation processors** — protobuf, OpenAPI,
   jOOQ — under Tier 1 slice 2's rules unchanged. Fourth because the sample
   may hold none of it: it is built against fixtures if so, and nothing
   before it depends on it. **Landed**, against fixtures: the sample applies
   none of the three. Each generator's default output directory is walked;
   no schema change.
5. **Semantic tokens for files over the guard** (`semanticTokens/range`).
   Fifth because it is a performance slice: the request exists for exactly
   the files the 640 KiB guard refuses, and the number decides it.
   **Landed**: one walk, which skips a node outside the range. A screen at
   the end of `Tier::Huge` is 2.46 ms p95, where `full` is guarded to
   empty. No schema change.
6. **The type hierarchy with JDK and JAR supertypes**, placed in the
   extracted sources since Tier 1's slices 1 and 4 gave them a location.
   Last because it is the smallest and the least read: a supertype a
   reviewer wants is usually one ctrl-click away already. **Landed**, which
   completes Tier 3: a supertype the workspace declares nowhere is listed
   where what was written certainly means it, placed in its extracted
   source, and expands to its own fact's supertypes. No schema change.

Authoring features stay out, as before.

### Tier 4 — decided 2026-10-12

Tier 3 completed the verbs version 1 lists. What is left is reading gaps:
places where definition, hover, a hint or references answer nothing on code
a reviewer reads every day. The order comes from counting each gap's shape
in the sample's 6 529 hand-written files:

| Shape | Count |
|---|---|
| Method reference, `Order::getId` | 3 162 |
| Block lambda, `x -> { … }` | 1 333 |
| `this(…)` / `super(…)` | 330 |
| `protected` field, read bare in a subclass | 257 declared |
| `collect(Collectors.…)` | 159 |
| `@SuperBuilder` / `@Singular` | 13 / 7 |

Seven slices, in this order:

1. **Method references in inference.** `stream.map(Order::getId)` is a
   `Stream<Long>`, so the chain after it and the next lambda's parameter
   type, as `map(o -> o.getId())` already does. `infer::bind` binds the
   functional interface's return variable from the referenced method's
   return type — static, bound, unbound-receiver and `::new` forms — where
   `sole_candidate` picks exactly one method for the interface's arity.
   First because it is the most frequent gap by a factor of two and every
   stream pipeline stops at one today. **Landed**: the one method of the
   name, fitting the form and the arity, binds what the interface returns;
   a raw generic type is read on the interface's first parameter; schema
   version 35.
2. **Block lambda return types.** `map(x -> { …; return y; })` binds like an
   expression lambda when every `return` in the body (not in a nested lambda
   or class) types to the same type. Any `return` that types nothing, or two
   that differ, binds nothing: equality, as `bind` already requires.
   **Landed**: a `null` result constrains nothing, and a block with a
   `return;`, none, or more than 32 binds nothing; schema version 36.
3. **Collectors through `collect`.** `collect(Collectors.toList())`,
   `toSet()`, `joining(…)`, `groupingBy(f)` and `toMap(f, g)` type their
   result. That needs the argument's own type arguments inferred from the
   receiver (`Collector<? super T, A, R>` against `Collector<T, ?, List<T>>`),
   which is the target typing `bind` refuses today. It is scoped to a
   generic method call passed where the parameter is a parameterized type,
   inferred by equality and never by a bound.
   **Landed**, for the variables the inner call's own arguments leave open;
   `? super X` binds its lower bound and `? extends X` nothing; schema
   version 37.
4. **Bare inherited fields.** A bare `count` in a subclass jumps, hovers and
   types as the supertype's field, and the worker records it, so references
   on a field list bare uses in other files. JLS 6.4.1 and 15.11 apply, the
   way static imports are already modelled: the innermost enclosing type
   whose list is whole and holds the field, and a local or parameter of the
   name always shadows it.
   **Landed**, as `infer::field_owner`; the worker visits every unbound
   expression name, so an on-demand static import's are references too;
   schema version 38. The sample's gain is in its tests, where a base class's
   `protected` field is written bare 201 times.
5. **References on library methods.** References on a JDK or JAR method
   list the workspace's calls to it — `repository.save(…)` for Spring Data's
   `CrudRepository.save`, which a reviewer asks about often. The worker
   records calls into JAR members, and calls into JDK members only for
   types the workspace extends or implements, so the table does not hold
   every `toString` and `equals`. The slice measures the table first and
   stops there if it doubles.
   **Landed**: JAR calls added 46% to the sample's table, under the line.
   A member with no workspace site is keyed by its owner and its place in
   the owner's facts, since a JAR member has no site until its sources are
   grafted; the JDK's closure rule kept 501 of 52 649 calls; schema
   version 40.
6. **`this(…)` and `super(…)` as constructor calls.** They jump to the
   constructor they select, hover it, get parameter hints and are listed by
   references, through Tier 3 slice 3's `sole_constructor` over the
   enclosing type's or the superclass's own constructors. An enum
   constant's arguments go through the same rule.
   **Landed**: 175 of the sample's 330 `super(`/`this(` jump, against
   none. The rest are a `protected` fixed-arity constructor beside a
   variadic one, a Lombok-written constructor, an implicit default one, or
   a JAR class's. An enum's written constructors are stored now, so schema
   version 41.
7. **`@SuperBuilder` and `@Singular`.** Lombok's builder for a class
   hierarchy, and the singular adders it writes for a collection field. They
   are last because they are rare in the sample, and they stay
   per-annotation rules in `lombok_members`.
   **Landed**, which completes Tier 4: all 312 links of the sample's
   `@SuperBuilder` chains jump, against none, and each of the 258 setters
   lands on its field. A `@Singular` field has its singular adder, its
   plural adder and its clear method. The builder types are stored with
   their supertypes and type parameters, so schema version 42.

**What the count dropped.** Downloading `-sources.jar`: 349 of the sample's
356 cached JARs already have one on disk. `@Delegate`, `@Accessors` and
`@FieldNameConstants` do not occur. An unqualified call inside an anonymous
class (125 anonymous classes) and a `switch` expression's type stay
recorded where their `ponytail:` is.

Startup speed is outside this tier: sharing the Gradle classpath across
worktrees and pruning old workspace caches are their own work, under
Phase 6.

Authoring features stay out, as before.

### Tier 5 — measured 2026-10-13

A wide probe of 30 of the sample's files answered 4 951 of 7 934 tokens.
Of the 1 124 real misses, the shapes were:
- about 900 library names with no sources on disk;
- 959 `case CONST` labels;
- about 200 three-argument slf4j calls, refused beside
  `info(Marker, String, Object)`;
- 191 calls with explicit type arguments, `Foo.<A, B>m()`.

1. **Downloading `-sources.jar`s.** Tier 4 dropped this, because 349 of 356
   JARs in Gradle's cache had sources. It missed the 19 classpath JARs Gradle
   takes from `mavenLocal()`: mockito-core, slf4j-api, commons-lang3 and
   others. For those, Gradle asks only the local repository for sources and
   finds none. **Landed**: the build tool fetches each JAR's sources once
   per classpath. Then `mvn` over a generated POM fetches what is still
   missing in a Maven repository, and both land where `sources_jar_for`
   looks. A probe over 13 files using Mockito, commons-lang3 and slf4j asked
   definition at 133 static calls: 126 jump into the fetched sources,
   against none.

2. **`case` constants.** Under an enum selector a label's simple name is
   the enum's constant (JLS 14.11.1). **Landed**: definition, hover and
   references answer it, and the scope answers a label only under a
   primitive, its box or `String`. 779 of 814 labels over 150 files answer,
   against 23.

3. **slf4j's overloads.** A `String` is no `org.slf4j.Marker`, because a
   JDK type's whole closure is the JDK's. **Landed**: `TypeWorld::is_subtype`
   answers `false` for a JAR or workspace target of a complete JDK closure,
   and elimination runs before fixed-before-variadic, so a ruled-out
   `Marker` overload no longer hides `info(String, Object...)`. 235 of 266
   `LOG.<level>(` calls over 40 files hover, against 103, and 45 that
   picked a `Marker` overload now pick the variadic one.

4. **Explicit type arguments.** `Foo.<A, B>m()` binds `m`'s own variables
   to what it writes (JLS 15.12.2.1), and a generic type's `@Builder` is
   generated. **Landed**: 97 of 125 calls after a `.<X>m(…)` over 115 files
   answer, against none. The rest are a record's `@Builder`, wildcard type
   arguments and JAR builders.

The other shapes are not ordered yet.

## Phase 0 — Foundation ← complete

Repository, architecture, CI, testing, benchmark harness, logging,
documentation, editor-integration skeletons.

**Done:** workspace and crate boundaries; configuration, error taxonomy,
cancellation and structured logging in `javelin-core`; the document store with
UTF-16 position mapping; Tree-sitter parsing; the persistent-cache layer; LSP
lifecycle and document sync; the CLI; Criterion benchmarks with size tiers and
percentiles; integration tests including a real LSP handshake; CI on Linux,
macOS and Windows; Zed and VS Code skeletons.

**Deliberately not done:** any Java semantics.

## Phase 1 — Interactive Java foundation ← complete

> Make a single Java file feel instant.

Every item below has landed and the editor pass in
[`manual-testing-zed.md`](manual-testing-zed.md) was walked by hand in Zed on
2026-09-03, which signs the phase off. The numbers are in that file's results
table. That pass is automated now — `crates/cli/tests/editor_pass.rs` and
`crates/cli/tests/reachability.rs`, run by `scripts/check.sh` — so a later
phase runs it rather than walking it, and this phase's three checks are the
three tests in that file's Phase 1 section.

- ~~Incremental reparsing~~ — done. `TextDocument::with_changes_reporting`
  reports the byte and point ranges each change touched, and `JavaParser::reparse`
  feeds them to Tree-sitter before reusing the previous tree. Edits carry the
  version they were computed from; a tree from any other version is parsed from
  scratch rather than silently corrupted.
- ~~A syntax-level view of a file: declarations, members, nesting.~~ — done.
  `FileSyntax` projects the tree onto named declarations with a span for the
  whole declaration and one for the name.
- ~~Local symbols and scopes within one file.~~ — done. `ScopeTree` records
  which names a file binds and over which byte ranges, with Java's shadowing and
  its type/variable namespace split.
- ~~`textDocument/documentSymbol` — outline.~~ — done. Hierarchical symbols
  built from the cached syntax view, answered at `Readiness::Syntax` from the
  open buffer alone. A file that is not open, or has not parsed, outlines as
  empty rather than as an error.
- ~~Definition and hover for identifiers declared in the same file.~~ — done.
  `javelin_parser::definition_at` resolves the identifier at an offset against
  the scope tree and the syntax view; definition returns a `Location` on the
  declaration's name, hover renders its source text and doc comment as Markdown.
  Both answer at `Readiness::File`, which a parsed file reaches. A name this file
  does not declare answers null rather than a guess.
- ~~Completion from local scope and file members.~~ — done.
  `javelin_parser::completions_at` offers the locals, parameters and fields
  visible at an offset plus the methods of the enclosing types, filtered by the
  partial identifier typed there, and returns them as an always-incomplete
  `CompletionList`: one file is never the whole answer. After a `.` or a `::`
  the answer is empty rather than every local in scope, because the receiver's
  type is Phase 2.
- ~~A worker pool, so parsing moves off the request loop.~~ — done.
  `didChange` updates the document store on the loop and hands the reparse and
  both view builds to a pool sized from `workers.backgroundThreads`. It selects
  between the client and the pool, so a request arriving mid-reparse is answered
  from the previous view instead of waiting. A result for a version the buffer
  has left is discarded, and a superseded reparse is cancelled rather than run
  to completion. `didOpen` stays synchronous: there is no previous view to
  answer from, and a thread hop is latency the first parse has nowhere to hide.

**Not met**, on generated god-classes only:

- Completion's p95 at `Tier::Huge` — 352 ms against < 50 ms — and its p99 at
  `Tier::Large`, 53.4 ms against the same 50 ms. Both because `FileSyntax` and
  `ScopeTree` are rebuilt whole per document version. Known, measured, and
  deferred: making those rebuilds incremental is its own piece of work and is
  not Phase 2's either.
- The < 10 ms first-parse budget at `Tier::Large` (25.9 ms) and `Tier::Huge`
  (269 ms). Not a Phase 1 regression — the from-scratch parse is untouched by
  this phase, and incremental reparsing changes what every keystroke *after*
  the first costs, not the first.

Those are Phase 1's numbers, on Phase 1's fixture. All four were re-measured at
the end of Phase 3a against a fixture that had grown to exercise 2b and 2c:
three are still missed and bigger, and the p99 at `Tier::Large` clears budget
at 42.5 ms, so it comes off the list. Numbers and the per-stage breakdown:
[`performance.md`](performance.md).

## Phase 2 — Semantic core ← complete

> Make Java semantics useful without requiring complete workspace indexing.

Type model, scopes and imports, member resolution, inheritance, generics,
overload resolution.

The constraint that shapes this phase: resolution must produce a useful answer
from one file plus whatever the index happens to have already, and must never
block waiting for indexing to finish.

2a, 2b and 2c have all landed and all had their editor pass run — 2a's on
2026-09-03, 2b's the same day against its own binary, 2c's on 2026-09-04
against its own, with every check in both of the latter two passing on the
first walk. 2c's was walked twice, over pipes and then in Zed, and the second
walk corrected the results table's startup column rather than merely confirming
the first: that column also counts the client's own delay before it sends
`initialize`. The phase is complete. Every check those three walks performed is
a test in `crates/cli/tests/editor_pass.rs` now, five per sub-phase.

- ~~Type model.~~ — done. `javelin_semantic::Type` and `resolve_written` turn a
  declared or written type — a field's, a parameter's, an expression's — into a
  type this file can reason about, resolved against `FileSyntax` and
  `ScopeTree` alone.
- ~~Member resolution, inheritance, generics.~~ — done, for a receiver whose
  type the open file declares. `type_of` infers the receiver's type from a
  closed set of expression shapes (a name, `this`, `super`, a field access, a
  call, `new`, a cast, an array access); `members_of` walks that type and its
  supertypes — as far as the same file declares them, with the file's own
  supertype-cycle guard and a node budget on generic substitution — and
  `member_completions_at`/`member_definition_at` answer completion, definition
  and hover from the result. An unqualified call used as a receiver — `g().` —
  is typed the same way when exactly one method of that name fits the call's
  arity. Navigating to the method itself was not; that is 2b's, immediately
  below, and it landed without `is_simple_name` having to change.
- ~~`var` inference and definition on an unqualified call (2b).~~ — done, under
  the same rule that already typed `g().`. `Decl::initializer` carries a
  projected initialiser expression for a declarator whose written type is `var`
  — which, in Java that compiles, is only a local — and for nothing else:
  `FileSyntax` is rebuilt whole per document version, so projecting an
  expression per declarator would put file-sized work back on the path that
  already misses completion's p95, and the guard is pinned by a test rather
  than left to be noticed. `infer::type_at_depth` types the local from that
  initialiser, spending a depth level on the hop, so a self-referential `var`
  terminates on `MAX_RECEIVER_DEPTH` instead of recursing. One rule, not three
  half-rules: a `var` with no initialiser, in a for-each header, or in a
  try-with-resources resource is unknown, because the last two would take their
  type from `Iterable<T>` or `AutoCloseable` and this phase cannot see either.
  (Tier 2's slice 8 types the for-each one.)
  On the other side, `javelin_parser::unqualified_call_at` finds the call under
  the cursor and `javelin_semantic::call_definition_at` answers it through
  `infer::sole_candidate` — the name, arity, varargs and shadowing filter
  member resolution was already applying, extracted so the two cannot drift
  into disagreeing about the same call. `handlers::resolve_at` wires it between
  member resolution and `javelin_parser::definition_at`, so definition and
  hover both answer from one change, and the unqualified call is resolved
  before `definition_at` runs — which is why `is_simple_name` still refuses the
  name of a `method_invocation` and no wrong jump is reachable.
  `call_definition_at` takes the *sole* candidate rather than the first: an
  unqualified call has no receiver to disambiguate it, so two candidates of the
  same name and arity answered nothing. 2c narrowed which calls that covers,
  below.
- **An unqualified call inside a nested or anonymous class.** Done for a
  nested class in Tier 1's slice 10: `infer::call_owner` walks the types
  around the call by JLS 15.12.1, pinned by
  `an_unqualified_call_inside_a_nested_class_reaches_the_outer_classs_method`.
  An anonymous class still stops the search, because the parser records its
  instantiated type as its name rather than its supertype.
- ~~Overload resolution (2c).~~ — done, over the subset of JLS 15.12 one file
  can demonstrate: applicability by subtyping (15.12.2.2) and by widening
  primitive conversion (5.1.2), run as **elimination**. `Expr::Call` now
  carries its arguments and `Expr::Literal` its kind, so `infer::type_of` can
  type an argument at all; `apply::excluded_by_arguments` drops a candidate
  when some argument demonstrably cannot be passed to the parameter in that
  position; and a lone survivor wins. `f(new Foo())` picks `f(Foo)` over
  `f(Bar)`, `f(1)` picks `f(int)` over `f(Foo)`, `f(null)` picks `f(Foo)` over
  `f(int)`, and a fixed-arity candidate beats a variadic one at the same
  argument count, which is 15.12.2's phase ordering rather than a preference.
  The qualified call runs through the same `infer::sole_candidate` as the
  unqualified one, which closes the exception this bullet used to record:
  `o.g(1)` against `g(int)` and `g(String)` answers nothing where it landed on
  whichever was declared first, and a field `g` declared above a method `g` no
  longer wins over the method, because the namespace the access asks for
  decides rather than declaration order. So the invariant is unconditional at
  last — *ambiguity answers nothing, never a guess* — on every path that
  produces a type and on definition and hover for a call, qualified or
  unqualified.
- ~~A candidate whose parameter type the open buffer cannot resolve.~~ —
  **closed by Phase 3a**, which is where it always belonged rather than being a
  gap in 2c. It was the largest thing 2c refused: `f(int)` against `f(String)`
  at `f(1)` answered nothing, because whether an `int` is passable to an
  unresolvable `String` is unknowable — the name could mean `Object`, `Number`,
  `Integer`, `Comparable` or `Serializable`, each of which takes an `int` by
  boxing. With a JDK indexed the name is not unresolvable: `int` boxes to
  `java.lang.Integer`, no fact makes an `Integer` a `String`, and the candidate
  goes. A `java.lang` table would have unlocked it a phase earlier and was
  rejected twice, in 2c and again in [ADR 0004](adr/0004-classpath-source-of-truth.md):
  it has none of the machinery to stay honest against a same-package sibling
  type, an on-demand import or a file-local `class String`, and it would make
  the invariant depend on a guess about what a name means. An overload pair
  differing only in a parameter type that comes from *another source file* is
  still refused, and that is 3b's.
- **Most-specific-method selection (JLS 15.12.2.5), and the refusals beside
  it.** Done where strict invocation proves it, in Tier 1's slice 8:
  `apply::most_specific` picks `f(int)` over `f(long)` at `f(1)` and `f(Dog)`
  over `f(Animal)`, and a string literal is a `java.lang.String`. What boxing
  or varargs decide is still refused, and the rest of this entry is the
  record of what was. All of it is recorded in `crates/semantic/src/apply.rs`, whose module
  doc names what falls outside the implemented subset and whose `ponytail:`
  comments name the ceilings that have an upgrade path. Without most-specific
  selection, `f(int)` against `f(long)` at `f(1)` and `f(Animal)` against
  `f(Dog)` at `f(new Dog())` leave two
  *applicable* candidates and answer nothing — the second is the most
  conspicuous refusal in the phase, because both types are declared in the
  file. Boxing and unboxing (5.1.7, 5.1.8) were not modelled in 2c and are
  **modelled as of Phase 3a**, wherever a type fact settles them: an `Integer`
  argument reaches `long` and not `short`, and a reference type the world holds
  that unboxes to nothing reaches no primitive at all. Where the world is
  silent no candidate is excluded on a ground boxing would reverse, which is
  2c's behaviour unchanged. A poly expression excludes nothing: a lambda and a
  method reference type as unknown, because their type depends on the choice
  being made. A declared type is excluded only when its whole supertype closure
  is known — which in 2c meant *file-local*, so a domain type with any
  supertype outside the file, `class Foo implements Serializable`, was
  separable from nothing. **Phase 3a closed that** for a supertype the
  classpath resolves; a supertype declared in another source file still aborts
  the walk with "could be", and that is 3b's. An anonymous class never
  excludes, because
  `FileSyntax` records its instantiated type as its name and never as its
  `extends`; without the guard, `f(this)` inside `new Marker() {…}` would be
  excluded from `f(Marker)`, a wrong pick rather than a missing one. Type
  arguments are not compared, so `Box<Foo>` and `Box<Bar>` are one
  declaration. A variadic candidate is checked on its fixed prefix only. And
  one refusal is *new* in 2c, traded for closing the shadowing rule's own
  guess: a generic override the subtype pinned down —
  `class Base<T> { void m(T t) }` with
  `class A extends Base<Foo> { void m(Foo f) }` — answers nothing, because
  own-shadows-inherited compares parameter types without applying
  `Member::subs`. Phase 3a widened one of them to cover an external type: an
  enum, a record or an annotation type **as the argument** excludes no
  external parameter type, whether the open file declares it or the classpath
  holds it. JLS 8.9, 8.10 and 9.6 give those three a *named* implicit
  supertype nobody writes down, so locally the walk has no
  `extends`/`implements` to follow, and externally the type fact's closure is
  marked incomplete and the world answers "cannot say". It runs one way only:
  the closure a walk drains is the argument's, so one of the three in the
  **parameter** position is still separable. An interface is not in the group
  in either position: its one unwritten supertype is `java.lang.Object`,
  which `TypeWorld::is_subtype` answers from JLS 4.10.2 without a fact.
- **Scopes and imports beyond the open file.** Not done in Phase 2, and
  **partly done in Phase 3a**: a name from an import, from the file's own
  package or from `java.lang` resolves when the JDK holds exactly one type it
  can mean. In 3a what it resolved *to* was a name, a kind and a supertype
  closure and not members, so nothing could be looked up on it; 3b's
  external-members slice closed that half. A name from another source file
  still answers nothing, and that is the rest of Phase 3b.
- **Semantic diagnostics — cut from this phase.** Under open-file-only
  resolution, every JDK type and every imported type is unresolved by
  definition. An "unresolved symbol" diagnostic built on that would mark most
  of a real file as broken, which is a false positive, not a warning. Still
  cut after 3a, for a narrower version of the same reason: a JDK-only index
  resolves `String` and not the project's own classes, so the false positives
  move rather than go away. Phase 3c, once the index is complete over
  everything a file can name.

## Phase 3 — Indexing

> Make workspace-scale navigation and references fast.

Split into four. 3a is a floor the rest stand on and could be built without
touching a single lifetime in `javelin-semantic`; 3b is the workspace's own
sources; 3c is the diagnostic that needs both of those plus Phase 4; and 3d is
the same index read backwards. Their execution order is not their numeric
order — see "Version 1 — what it is for" above.

### Phase 3a — the classpath floor ← complete, editor pass signed off 2026-09-08

> Make a name the open file does not declare mean something.

- ~~JDK discovery.~~ — done. `javelin_classpath::discover` reads directories
  only: a configured home, `$JAVA_HOME`, configured search paths, then the
  built-in locations, taking the highest version within one expansion. It never
  spawns `java -version` or `/usr/libexec/java_home` — a process spawn buys
  nothing reading the `release` file does not, and it would make the function
  untestable.
- ~~Type facts from the JDK.~~ — done. `read_type_facts` parses
  `lib/src.zip` with the same Tree-sitter parser the editor's buffer goes
  through and emits one `TypeFacts` per declared type: fully-qualified name,
  simple name, kind, type-parameter count, and direct supertypes resolved to
  fully-qualified names. A supertype that resolves ambiguously or not at all is
  dropped and `supertypes_complete` goes false, which the subtype question
  reads as *cannot say* rather than *no*. Why src.zip and not class files:
  [ADR 0004](adr/0004-classpath-source-of-truth.md).
- ~~One name-resolution rule.~~ — done. `javelin_classpath::simple_name`
  resolves a name as written against a table of fully-qualified names, in
  tiers: single-type imports, the file's own package, explicit on-demand
  imports, then `java.lang`. The first tier with a hit decides, and two hits
  inside one tier answer `Ambiguous`, which every caller turns into *nothing*.
  Both the src.zip reader and the server resolve through it, so the invariant
  *ambiguity answers nothing, never a guess* has one enforcement point.
- ~~A persistent fact cache.~~ — done. `javelin-index` writes the facts once
  and reads them once, keyed by the JDK's home and version, at
  `cache_root/jdk.sqlite` — under the cache root rather than a workspace
  directory, so one 10.95 s read warms every project on the machine. The
  request path never queries SQLite: the facts are published as an immutable
  `Arc<TypeWorld>`.
- ~~Background, never blocking.~~ — done. The read starts from the
  `initialized` notification on a detached thread, and the finished world
  arrives on a channel the request loop already selects over. Nothing on the
  path to the `initialize` response opens the zip, spawns a process or walks a
  filesystem for a JDK; process startup measures 10 ms against 11 ms for a
  binary built from the parent commit. The thread is never joined, because a
  shutdown that waited on a read that is seconds from finishing is the "server
  did not shut down" dialog.
- ~~The engine's window onto it.~~ — done. `javelin-semantic` declares the
  `Externals` trait and `javelin-server` implements it over `TypeWorld`, so the
  engine stays testable against a hand-written world and the crate graph stays
  acyclic. A world with no facts in it answers "cannot say" to everything,
  which is Phase 2's behaviour exactly and is what a machine with no JDK gets:
  `ServerState` holds a `ClasspathExternals` over `TypeWorld::empty()` from
  construction, and swaps in a full one if a read succeeds. `NoExternals` is
  the engine's own test double for the same behaviour, and the server never
  constructs it.
- ~~`Readiness::Dependencies` becomes real.~~ — done. It is reached,
  observable and logged. `Answer` is *not*: it is constructed on the seam and
  read by nothing on the wire, because in *that* phase the classpath
  contributed nothing to a completion list, so deriving `isIncomplete` from its
  arrival would have shipped an empty list marked complete. 3b's
  external-members slice is what made an honest predicate expressible;
  `Answer` is still read by nothing on the wire.
- **Members of an external type.** Not done in 3a, and the visible refusal:
  completion after `s.` on a `String` was empty even though the server knew
  the type. Closed by 3b's second slice, below.

### Phase 3b — workspace sources, external members and a jump into them

> Make the whole project navigable, and make an external type answerable.

- ~~`Span` becomes `(FileId, Span)` throughout `javelin-semantic`.~~ — done.
  A declaration is a `DeclSite` of a file and a span, a query carries a `Files`
  set of the files it may reach and which of them it is in, and both supertype
  walks key their cycle guards on the site and resolve each frame's written
  supertypes in the file that wrote them. It changed no answer — the server
  builds a single-file set — and it left members of an external type refusing
  for one reason only, `TypeFacts` carrying no members, which is what the next
  slice went and fixed.
- ~~Members of an external type.~~ — done, and it is the first thing since 3a
  an editor can see. `read_type_facts` now records a `MemberFacts` per member a
  type declares — name, kind, staticness, declared type, parameter types in
  order, variadicity — with each type name resolved at read time as far as it
  resolves, through the
  same `simple_name` the supertype pass uses, rather than stored as written
  text and resolved on the request path: that would need each declaring file's
  package and imports in the cache and would put name resolution on the
  request loop. `javelin-index` stores them in a `members` table, schema
  version 4 at the time and 5 since the third slice; a type variable, an array
  or an ambiguous name stays `MemberType::Unresolved` and types as unknown.
  `Externals` gained `members(fqn)`; `javelin-server` answers it by
  closing over the world's supertype graph, and `members_of` merges that list
  with the file-local walk's, so a buffer-declared class extending a framework
  one answers with both halves at once. Completion after `s.` on a `String`
  offers `length`; hover renders a member from facts, with no parameter names
  and no prose, because a fact carries neither; definition refused, on two
  grounds at once — `src.zip` was nowhere a client could be sent and
  `Definition` carried no file — which the third slice closed.
  The read grew 10.95 s → 11.0–11.4 s and the fact cache 5.7 MB → 27.7 MB,
  which [performance.md](performance.md) measures.
- ~~Deriving completion's `isIncomplete` from something true.~~ — done, in the
  same slice. It was `true` unconditionally; it is now a conjunction of five
  proofs — the classpath has arrived, the answer came from the member query,
  the receiver's type resolved, the member walk reached everything the type
  names, and the list was not cut off at the candidate cap — and anything
  unproven leaves it `true`. It reaches `false` for an external receiver and
  almost nowhere else, because a file-local receiver's list is short by
  `java.lang.Object`'s members: nobody writes that `extends` and the walk
  follows written supertypes only.
- ~~`Definition` carrying a `FileId`, ahead of that index, and a jump into the
  JDK's own sources.~~ — done, the third slice. `javelin_parser::Definition`
  carries the document its spans are offsets into and `handlers::definition`
  builds its `Location` from that rather than from the URI the request named,
  which is what stops a foreign span being rendered against the requested
  buffer. On the classpath side a `TypeFacts` now carries the `src.zip` entry
  it was read out of and a `MemberFacts` the byte offsets of the member's own
  name inside it, both persisted (schema version 5) and both carried across the
  `Externals` door as an `ExternalSite` — filled from the frame that *declares*
  the member, so an inherited one names its supertype's file.
  `crates/server/src/jdk_sources.rs` extracts that one entry under the cache
  root, keyed by JDK identity as the fact cache is, and the handler converts
  the offsets against a document over the extracted text. So `s.length()` for a
  declared `String s` jumps into the JDK's `String.java`. An entry name with a
  `..`, a leading `/` or a backslash is refused rather than extracted; the
  copy is writable and an edit to it means nothing, which is `ponytail:`-marked;
  and an external *type* name still answers nothing, because a fact records no
  span for the type's own name. The cache grew 27.7 MB → 30.7 MB and the first
  jump of a session costs ~24 ms, almost all of it opening the archive, which
  [performance.md](performance.md) measures.
- ~~A persistent index over the workspace's own sources.~~ — done, the fourth
  slice, and it changes no answer. `javelin-index` walks the folders the client
  named at `initialize` — pruning dot-directories and `target`, `build`, `out`,
  `bin` and `node_modules`, following no symlink, honouring the configured file
  size cap — parses every `.java` it takes and records what each declares: a
  package per file, and per type a fully-qualified name, a simple name, a kind,
  the spans of its name and of the whole declaration, and its supertypes **as
  written**. Schema version 6, in a database of its own under
  `paths::workspace_cache_dir`, because these facts belong to one project where
  the JDK's belong to the machine. The walk is incremental: a file whose
  modification time and size match its stored row is not opened, which is
  12.4× on a thousand-file tree — 82.5 ms cold against 6.68 ms warm, which
  [performance.md](performance.md) measures. `crates/server/src/workspace.rs`
  runs it on a detached thread from the `initialized` notification, cancellable
  and never joined, and publishes an immutable `Arc<WorkspaceIndex>` the
  request path can ask instead of SQLite. Nothing asks it yet: no handler is
  touched, `Files` is still single-file everywhere the server builds one, and
  the observable surface is a `workspace ready` log line and two new lines in
  `javelin doctor`. Members are deliberately not recorded — the schema grows
  when a feature can say what shape it needs — and the index describes what is
  on *disk*, so an unsaved buffer is not in it.
- ~~Workspace symbols — the first reader of that index.~~ — done, the fifth
  slice, and the first answer in the project's life that is not about the open
  buffer. `workspace/symbol` is advertised and routed, and
  `WorkspaceIndex::matching` answers it: an ASCII-case-insensitive substring
  of a type's *simple* name, prefix matches first so the 500-result cap falls
  on the right side of the list, no ranking beyond that because every client
  with a symbol picker re-ranks what it is handed. 3.8–8.9 µs over 2 000
  indexed types, which [performance.md](performance.md) measures and which is
  the row that says when the linear scan has to become a prefix index.
  Three scope decisions, all of them visible: **only types**, because only
  types are indexed — a method name finds nothing, and the schema grows when
  the slice that wants members can say what shape it needs; **the answer is
  disk**, so a type declared in an unsaved buffer is absent and one whose
  declaration has moved is listed at the line it used to be on, which is the
  one *wrong* answer the slice ships and is `ponytail:`-marked on the handler;
  and **no open-buffer preference**, deliberately, because preferring the
  buffer for positions and the index for membership is half a reader and the
  other half is re-indexing on save. The index stores a name's line and
  character beside its byte offsets for this — the conversion is free on the
  walk's thread, where the document exists, and would be a whole file read on
  the request loop. Schema version 6 → 7.
- ~~Cross-file definition and hover on a *name*.~~ — done, the sixth slice,
  and the first answer that resolves a name across files rather than listing
  one. The index arrives through `Externals` rather than through a window of
  its own, which is the question the fifth slice left open: `ClasspathExternals`
  now answers `resolve` out of one `TypeTable` over the workspace's own types
  *and* the JDK's, so `javelin_classpath::simple_name`'s JLS 7.5 tiers rank
  the two halves against each other — a sibling in the file's own package
  outranks `java.lang`, and two candidates in one tier are ambiguous whichever
  half each came from. `javelin-semantic` gained one query,
  `external_type_at`, the type-level sibling of `external_member_at`: it
  answers what the *name* under the cursor means, as a fully-qualified string
  and nothing more, because this crate has no file for that declaration. The
  server turns that into a place — `WorkspaceIndex::with_fqn`, exactly one hit
  — so `Greeter` in one file jumps to `Greeter.java`'s own name and hovers as
  `class com.example.Greeter`. Two files declaring one fully-qualified name
  answer nothing. A JDK type's name still answers nothing either, for the
  reason the third slice left: a `TypeFacts` records no span for it.
  The index is asked for *names* only. Its `is_subtype` and its `members` are
  still the JDK's alone, deliberately — see the last bullet — so a `.` on a
  cross-file receiver still offers nothing, and no overload is excluded on the
  strength of a workspace fact. The jump's position is as old as the last
  walk, the same ceiling `workspace/symbol` carries and the same upgrade
  closes it.
- ~~Find-references.~~ — done, the seventh slice, answered by one rule: a
  token is a reference to a target exactly when go-to-definition at it lands
  on the target. A *type* is answered across every indexed file — the walk
  records every plain name a file writes where a type could be meant, with the
  file's imports, schema version 7 → 8, and a query resolves each row now,
  against its own file's package and imports, through the same
  `Externals::resolve` definition uses. Resolution is at query time because
  the table the tiers read changes after the walk. *Anything else* the open
  file declares is answered in that file, call sites and `this.` selections
  included. Qualified names and import lines are not counted, because
  definition refuses those positions too; another open buffer's unsaved edits
  are read from disk; and a member is referenced in its own file only, which
  is what the next bullet's members fix.
- ~~Cross-file *member* navigation.~~ — done, the eighth slice. The walk
  records each workspace type's members with their types as written, through
  the same reader the `src.zip` pass uses, schema version 8 → 9, and
  `Externals::members` closes over workspace and JDK types together, resolving
  a workspace frame's types against its own file when a query visits it. A `.`
  on a cross-file receiver completes, hovers and jumps; an unqualified call to
  an inherited member answers too; `workspace/symbol` lists members.
  `is_subtype` stays the JDK's, which is what keeps `apply.rs` sound.
- ~~Cross-file member references.~~ — done, the ninth slice. The workspace
  worker resolves every member selection of every file with definition's own
  chain and stores the ones that land on a workspace member, keyed by that
  member's site, beside every name the resolution asked about, resolved or
  not, schema version 10 → 12. A batch re-resolves its changed files and every
  file that depended on a type they declared, before or after the change; a
  file an interrupted session left unresolved is resolved at the next startup,
  and the JDK world's arrival re-resolves what was resolved without it. References on `greet` in
  `Greeter.java` list `greeter.greet()` in a `Reception.java` never opened.
  Calls into the JDK are not recorded, a bare field name is not a selection, overrides are separate targets, and the key is a byte offset
  that is exact once the declaring file's re-index has landed — each
  `ponytail:`-marked.
- The *local* cross-file definition refusal is what remains in the phase, and
  it is no longer about addressing — `Definition` carries a file — but about
  rendering: `Definition::signature` slices a document and a `Files` set
  carries none. Nothing a user can reach meets it, because every `Files` the
  server builds is single-file and a cross-file answer goes through the
  index.
- Two `ponytail:` comments in `crates/semantic/src/apply.rs` are addressed to
  this phase by name, and both are still open — but they are no longer at risk
  of being *silently* wrong, which is what the sixth slice had to settle. The
  `(External, Declared)` arm and `may_reach` both assume an external fact came
  from the JDK and therefore cannot lead back into the open buffer, and a
  `Type::External` can now name a sibling source file. Both survive because
  they read `Externals::is_subtype`, which the workspace half deliberately does
  not answer: a workspace fqn is not *known*, so the arm excludes nothing and
  the walk treats the supertype as opaque and stops. Refusing is the safe
  direction, and it is bought by keeping half the index behind the door rather
  than by the arms knowing what they are looking at. The upgrade they name is
  still the right one — a fact that says where it was read — and half of it
  landed with the third slice.

### Phase 3c — semantic diagnostics ← two slices landed 2026-09-28

> Say "unknown symbol" only where it is true.

**Landed second: unresolved *methods*.** A call `receiver.name(…)` whose
receiver's type is certain, whose member list `walk_members` reports whole in
both halves — through `MemberWalk::closed`, which ignores only the nested
types an external static half withholds — and which has no member called
`name` of any kind is a warning with the code `unresolved-method`. Name
absence only: another arity or overload, a field of that name, a static
method through a value are silent, because judging which a call reaches is
overload resolution. Methods only: a field access, an unqualified call and a
method reference are silent. The receiver is typed by narrow rules — a
written type, a `var` with a certain initialiser, a field only in its own
declaring type, a bare name as a type, `this`, an unqualified `super`, a
cast to one type, an unqualified non-anonymous `new`, and a chain hop only
where every candidate of that name and arity returns one type and none is a
workspace member, whose type another file resolved blind — and the server
refuses a chain whose names, a file-local receiver's written supertypes
included, an inherited nested type, a single static import, an inherited
field or a generated source may shadow, and a receiver the judged file itself
declares, whose `private` members the lists leave out. Wholeness was widened
at the sources (schema 14): a JDK entry that parsed with errors, a source enum
or record, a multi-release overlay and a workspace file that parsed with
errors all make a list short, and so do a workspace copy of a JDK or JAR type
and a workspace type whose supertype resolved where an enclosing type's
inherited member types or a static import could have meant another. A member
generator — Lombok, Manifold, AspectJ's compiler, by build file, classpath,
import or `.aj` source — and an index behind a buffer withhold the member
half, a buffer that differs from the disk when it is opened included; a
build file changed during the session withholds every judgement until the
next. Below
the JDK's own feature version, no receiver whose closure reaches a JDK type
besides `Object` is judged, because what the JDK removed since is callable at
the project's level. Fields, qualified names and the file-local `Object` gap
are the ceilings, each `ponytail:`-marked.

**Landed: unresolved *types*.** A type name the file writes in an unambiguous
type position — a `type_identifier` or an annotation's name, unqualified, bound
by no scope and declared nowhere in the file — that `Externals::resolve`
answers *unknown* for, against the JDK, the JARs and the workspace, is
published as a warning with the code `unresolved-type`. Ambiguous is not
unknown and is never reported. The world must be whole first: the JDK in, the
workspace walked with no file skipped for its size and no Kotlin, Groovy or
Scala source beside it, and the dependency classpath complete — a project
with no build file, or one whose build tool ran (`build.importOnStartup`) and
succeeded and whose every JAR was read. So **a Maven or Gradle project with
the import off is never diagnosed**, which is the default. A file is judged
only when it parses cleanly, has no static on-demand import, every on-demand
import names something held, and every type it declares has a whole
supertype closure; inside it, a nested type of any frame of those closures, a
name inside a local or anonymous class body, a contextual keyword, a static
single-type import and a generated-looking name are silent. The judgement
runs on a thread of its own, debounced by `diagnostics.debounceMs`, and is
published only for the version it was computed for; a world that stops being
whole clears what was published. The review pass added the gates that each
closed a false positive: a package the walk prunes by name, a build or a JAR
beside loose sources that nothing asks, a Gradle dependency skipped
leniently, a file outside the walk, a class a JAR reader could not read, a
processor path or a source generator only the build file shows, a Java level
the JDK does not match, a compact source file, `buildSrc` and included
builds, and a client that does not watch the files. Each ceiling is
`ponytail:`-marked:
members are not diagnosed, a qualified name with a type prefix is not
(one a package qualifies is since Tier 1's slice 8), a JDK newer than the
project's resolves names the project cannot see, a file that grows past the
size limit is noticed at the next re-walk, and when a JAR registers an
annotation processor any name containing a workspace type's simple name is
silent. What follows is the account this section was written as.

Cut from Phase 2, still cut after 3a, and promoted by the version 1 decision
from "the thing that comes after indexing" to the reason indexing is being
built. It is the one feature that answers a question rather than a navigation,
and it is what makes reading generated Java faster than reading it twice.

**What it says.** One diagnostic kind: a name in this file resolves to nothing.
A type name no file, no import and no JAR accounts for; a member selected on a
receiver whose type is known and whose member list is known to be whole. That
is the whole vocabulary.

**What it must never say.** Anything it is not sure of. The constraint from the
version 1 section is the design: a false positive costs more than ten missing
answers, because a reviewer who is shown one wrong squiggle stops reading all
of them. Three gates follow from it, and none is optional.

- *Completeness before judgement.* A file is diagnosable only when everything
  it names could in principle have been indexed: the JDK facts are loaded, the
  workspace walk has finished, and the dependency classpath from Phase 4 has
  resolved. Any one of those outstanding and the file gets no diagnostics at
  all — not a partial set, because a partial set is indistinguishable from a
  complete one to the person reading it.
- *Wholeness before a member verdict.* `members_of` already computes whether
  the list it produced was whole, and `members_of` already discards it — the
  soundness ceiling the `ponytail:` on `members_of` in
  `crates/semantic/src/members.rs` records. A member diagnostic is the one caller that
  *must* have it, and unlike `sole_candidate` it can afford it: refusing to
  diagnose is a missing squiggle, where refusing to pick is a lost navigation.
  This is the slice that gives that flag a reader, and it reads it in the
  direction that is safe.
- *Silence on everything unmodelled.* Every refusal `crates/semantic/src/apply.rs`
  records deliberately — poly expressions, type arguments, variadic tails,
  most-specific-method selection, the enum/record/annotation supertype gap —
  is a case where the engine knows it does not know. Each one is a diagnostic
  that is not emitted, and the module doc there is the checklist.

**What it needs that does not exist.** `textDocument/publishDiagnostics`, which
is the first *notification* the server sends rather than answers, and so the
first thing on the seam that `Answer` was built for and nothing reads. A
debounce, because diagnostics recompute per version and a fast typist produces
versions faster than a file can be walked. `didSave` and
`didChangeWatchedFiles`, shared with Phase 3d. And a decision about scope:
open files only is cheap and is what a reviewer needs; the whole workspace is
a background job with a progress notification and is not version 1.

### Phase 3d — the inverse edge ← complete 2026-09-28, not signed off

> Ask what implements this, and what calls it.

Track B's tail, and it is the same index read backwards. Phase 3b's references
slice built an occurrence table; the supertype edges are already stored. What
is missing is only the inverse of each: subtypes of a type, and callers of a
method.

- ~~`textDocument/implementation`, `typeHierarchy/*`, `callHierarchy/*`.~~ —
  done. A subtype is found by the simple name it writes and kept when the
  member closure's own resolution says what it wrote means the type; an
  override matches by name, arity and resolved parameters, a type variable
  matching anything, and two matches is ambiguity. Callers are the member
  references grouped by the member whose declaration holds each, schema
  12 → 13. A JDK or JAR supertype has no location and is not shown; calls into
  the JDK are not recorded and are not outgoing calls. The account the
  bullets below were written as:
- `textDocument/implementation` — the inverse supertype edge. The one feature
  on this list a reviewer of generated code reaches for by reflex, because
  generated code is full of interfaces with exactly one implementation and the
  interface is never the interesting file.
- `typeHierarchy/*` and `callHierarchy/*` — both edges again, walked
  transitively and answered lazily per expansion, which is what the LSP
  protocol's two-step shape is for.
- ~~Re-indexing on save, `didSave` and `didChangeWatchedFiles`.~~ — done,
  ahead of the rest of the phase because a reviewer switching branches meets
  it first. The workspace thread stays alive after its walk, owns the store,
  and re-indexes a saved file, a watched change on disk and an open buffer's
  unsaved text after a quarter-second pause, publishing one fresh index per
  pause; a closed buffer goes back to disk. A buffer's rows are stored under a
  time and size no file has, so the next walk re-reads the disk. The watcher is
  registered with `client/registerCapability`, the first request the server
  sends, only for a client that says it supports that. What follows is the
  account this bullet was written as:
- Re-indexing on save, `didSave` and `didChangeWatchedFiles` — which is the
  staleness ceiling `handlers/workspace_symbol.rs` and `handlers::workspace_type_at`
  both carry a `ponytail:` for, and which the version 1 decision promotes from
  a known shortcut to a defect. A jump that lands on the line a declaration
  used to be on is a wrong answer, and a reviewer meets it constantly because
  they are reading files they never opened and never edited. Preferring the
  open document for positions is the other half and lands with it;
  `a_type_declared_only_in_an_unsaved_buffer_is_not_offered_to_the_picker` is
  the check that is supposed to fail the day this ships.
- A file created, deleted or renamed on disk reaches the index, which today
  nothing does.

## Phase 4 — Dependency classpath ← first slice landed 2026-09-28

> Make a name that came from a JAR mean something.

**Landed:** the build tool is asked and the JARs are read. `javelin-build`
runs the project's `mvnw`/`gradlew`, or `mvn`/`gradle` on `PATH`, in a process
group of its own on the classpath thread after the JDK's world is out, reads
the classpath off its output, and caches it keyed by every build file's
modification time and size — never believed for a build naming a version
that can move on its own, a range or a dynamic version, nor for a Gradle
answer naming a `SNAPSHOT` JAR.
`javelin-classpath` reads each JAR's class files
into `TypeFacts` — member types from `Signature`, so a type variable is never
its erased `Object` — stored per JAR under the cache root. The server
publishes the JDK's world and then the JDK's world extended with every JAR's,
the JDK winning a clash, two JARs clashing with different APIs leaving the
kept fact incomplete, and a JAR's facts never answering subtyping. A JAR
type completes and hovers; definition into one answers nothing, because a
class file has no text. ADR 0005 records why a class file is not a second
front end, and why running the build tool — which runs the project's code —
is off until `build.importOnStartup` turns it on. **Still owed:** `-sources.jar` for javadoc, parameter
names and a jump; multi-module source roots for the walk; and re-resolving
member references once the JARs arrive.

Renamed from "Build systems", because what version 1 needs from a build system
is one list of paths, and everything else a build system offers is Phase 6 at
the earliest. This is Track A's long slice and the thing Phase 3c stands on: in
a real project most of what a file names came from a dependency, so an index
without one cannot be complete about anything, and a diagnostic built on it
would mark every Spring annotation as unknown.

**Do not reimplement the build tool.** Maven's dependency mediation, BOM
handling, profiles and property interpolation are a project on their own and
getting them subtly wrong produces a classpath that is wrong in the way that is
hardest to debug. Invoke the tool instead:

- Maven: `mvn -q dependency:build-classpath -Dmdep.outputFile=<tmp>`.
- Gradle: an init script that prints the runtime classpath. The Tooling API is
  the other option and it is a large dependency for one question.

Both cost seconds to tens of seconds, both require the tool on `PATH`, and
neither may ever gate an answer. The architecture for that already exists and
has been used twice: detached thread from `initialized`, result cached on disk
and keyed by the build files' modification time and size — the same
incrementality rule `walk.rs` uses — published as an immutable snapshot the
request path reads. With no tool on `PATH`, no build file, or the resolution
still running, the answer is exactly what it is today: the JDK and the
workspace, and every feature degrades to Phase 3b rather than failing.

**Then a `.class` reader**, which is what the resolved JAR paths are full of.
Its scope is much smaller than it sounds: the constant pool, `this_class`,
`super_class`, `interfaces`, access flags, each field and method's `descriptor`,
and the `Signature` attribute where generics need it. No `Code` attribute, no
stack maps, no verification, no bytecode of any kind. It produces the
`TypeFacts` and `MemberFacts` that already exist and stores them in the cache
that already exists, alongside the JDK's.

**An ADR is owed here**, because "Javelin reads class files" reads like a
contradiction of [ADR 0004](adr/0004-classpath-source-of-truth.md)'s "no second
Java front end", and it is not one. A class file is a *declaration* format:
already resolved, already erased where it is going to be erased, nothing to
name-resolve and nothing to type-check. Reading one is closer to reading the
fact cache than to parsing Java. ADR 0005 says that, and says why `src.zip` is
still the source of truth for the JDK — the sources carry javadoc and
parameter names and the class files do not.

A `-sources.jar` beside a dependency is read when present, for javadoc and
parameter names only. It is an enrichment and never the source of truth,
because it is frequently absent and occasionally stale, and a hover that is
missing prose is a smaller failure than a member list that is missing a method.

**Not in this phase:** annotation processing, generated sources beyond the
conventional output directories, running a build, incremental compilation, or
anything that writes to the project. Multi-module source roots are in, because
the workspace walk needs to know which directories are source and a monorepo
without them indexes its own build output.

## Phase 5a — Reading aids

> Make a file readable before anything has been resolved.

Track C, and it shipped first despite the number because it needs no index and
no classpath — every one of these falls out of the `FileSyntax` and
`ScopeTree` that a file has milliseconds after it is opened. Four of its five
things landed; the fifth did not, for a reason worth stating plainly below.
The phase is not signed off: its four checks pass in the automated editor
pass, and sign-off is a human opening Zed, which building this phase did not
do.

- ~~`textDocument/documentHighlight`~~ — done. Every occurrence of the name
  under the cursor, within the file, resolved through `ScopeTree` at each
  occurrence's own offset and kept only where the resulting `Binding` matches
  the one under the cursor — so a shadowing inner `int x` and an outer one are
  two bindings, not one, and highlighting one leaves the other dark. It covers
  what `ScopeTree` binds and nothing else: a method's or a constructor's own
  name highlights nothing, because neither is bound there — matching a call
  site to a declaration is overload resolution, not a scope lookup — and that
  refusal is `ponytail:`-marked in `crates/parser/src/queries.rs`.
- ~~`textDocument/semanticTokens/full`~~ — done, `full` only, exactly as
  planned: no `delta`, no `range`. Twelve token types from two sources — the
  tree directly for comments, literals and keywords, `ScopeTree` for
  identifiers resolved at their own offset — and an identifier that resolves
  to nothing emits no token at all, the same "ambiguity answers nothing"
  invariant the rest of the engine runs on. Its benchmark missed the 50 ms
  budget at two tiers; see below for what that means and does not mean.
- ~~`textDocument/foldingRange` and `textDocument/selectionRange`~~ — done,
  both direct from the tree: bodies, block comments and runs of imports for
  the first, the named-node ancestor chain from the cursor outward for the
  second.
- `textDocument/inlayHint` — **not done, and not moving into a later phase
  under this one's name.** Parameter names at call sites need a parameter
  *name* beside its type on `MemberFacts`, which is a `javelin-classpath`
  change, a schema version bump and an eleven-second re-read of the JDK's
  `src.zip` to backfill it — Track A work wearing Track C's clothes, not the
  free ride the other four were. It waited for parameter names on `MemberFacts` and landed with Tier 1.s
  slice 5.

**The ceiling this phase was expected to turn into a blocker did not, and the
benchmark found a different one instead.** `FileSyntax` and `ScopeTree` are
rebuilt once per document version on the parse pool whether or not anything
asks for tokens; a `semanticTokens/full` request reads the already-installed
`FileView` and rebuilds nothing, so shipping it changed nothing about that
cost. Incremental view rebuild stays exactly where it was — an open ceiling
no phase owns. What
`benchmarks/benches/semantic_tokens.rs` found instead is `ScopeTree::find`'s
own cost (`crates/parser/src/scopes.rs`): a linear, reversed scan of one
scope's bindings. The fixture declares one field per generated method, all in
the same flat class-body scope, so resolving every field's declaration and its
one use against a scope of 20 000 bindings is quadratic in the field count
rather than linear in the file — p95 78.29 ms at `Tier::Large` (2 000 methods)
and 5.47 s at `Tier::Huge` (20 000), against the 50 ms budget, where
`Tier::Small` and `Tier::Medium` meet it by 300× and 15× respectively.
`semanticTokens/full/delta` — the fix this section used to name — would not
close it: LSP's delta variant still computes the full current token list
before diffing it against the previous `resultId`, so it pays the same cost
this handler does. The real upgrade, named on the handler's own `ponytail:`,
is inside `ScopeTree` itself — an index per scope, or memoising a resolution
repeated within one walk — and it is recorded rather than acted on here, per
Ruling 2: a missed budget is a recorded number, not a trigger to build
something in the same slice that found it.

It was acted on one slice later, by 3b's references slice, whose workspace walk
resolves every identifier of every file and could not afford the quadratic
scan: `ScopeTree` indexes each name's bindings and binary-searches a scope's
children, and `FileSyntax::declaration_at` descends by span. The walk is
linear — 30.5 ms at `Tier::Large`, 357 ms at `Tier::Huge` — and the size guard
moved from 128 KiB to 640 KiB, so `Tier::Large` is coloured and `Tier::Huge`
is still refused.

Two of the four needed a new `SURFACE` row in
`crates/cli/tests/reachability.rs` — `foldingRange` and `selectionRange` — the
rule that exists because Phase 2a shipped a feature unreachable for a whole
phase. The other two, `documentHighlight` and `semanticTokens/full`, were
already listed there unadvertised, the way `workspace/symbol` was:
implementing them flipped the same assertion the other way rather than adding
a new row.

## Phase 5b — Authoring features ← out of version 1

Rename, code actions and quick fixes, organize and auto-import, signature help,
refactoring, and the owed completion upgrades. All of them are an author's verbs and none is on
version 1's path. Rename in particular is cheap *after* Phase 3b's references
slice — it is that table plus a `WorkspaceEdit` — and it is still out, because
a tool that renames is a tool that writes to the project and that is a
different set of promises than a tool that reads it.

Formatting is not here and not anywhere. The reason is in the version 1
section.

## Phase 6 — Compatibility and optimisation

Large real-world projects and monorepos, memory optimisation, startup
optimisation, indexing optimisation, compatibility edge cases, editor-specific
polish.

Two items arrive here with a known trigger rather than as general intentions:
`WorkspaceIndex::matching` is a linear scan over every indexed simple name,
which is 8.9 µs over 2 000 types and about 4 ms over a million, so the prefix
index its `ponytail:` names is owed when a monorepo is the test case rather
than before. And an edit arriving before the previous reparse lands falls back
to a full parse, which re-scans the file while a fast typist types; accumulating
edit batches per document against the installed version is the upgrade.
