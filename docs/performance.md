# Performance

Performance is a product requirement in this project, not an optimisation phase.
A Java language server that is correct and slow has already lost to the one the
user is currently tolerating.

## Budget

Targets, not guarantees. They exist so a regression is a visible event rather than a slow drift.

| Operation | Target |
|---|---|
| Process startup (exec to ready) | < 50 ms |
| `initialize` round trip | < 100 ms |
| First parse of an open file | < 10 ms |
| Local completion | < 50 ms |
| Go to definition | < 50 ms |
| References within a file | < 100 ms |
| Warm startup (cache present) | hundreds of ms |

Interactive operations are judged at **p95**, not the mean. A good average with a bad tail is what "it feels sluggish" actually is.

## What is measured

The harness in [`benchmarks/`](../benchmarks) covers, or reserves a place for:

| Measured today | Reserved |
|---|---|
| Cold process startup (`scripts/bench-startup.sh`) | References |
| `initialize` round trip, with percentiles | Workspace symbols |
| Engine construction | Diagnostics |
| Parsing, across four size tiers | Workspace source indexing |
| Document open | Memory usage, under an allocator profiler |
| Single-keystroke edit, full vs. incremental reparse | |
| Position lookup in a large file | |
| Completion, warm and one keystroke after an edit, unqualified and after a `.` | |
| Completion after a `.` on a receiver typed from the classpath | |
| Definition, on a local, a field and a call | |
| A jump into the JDK's own sources, cold and warm | |
| Argument-type elimination, refusing and resolving | |
| Request latency while a `Tier::Huge` reparse runs | |
| Reading a real JDK's `src.zip`, by hand — an `#[ignore]`d test | |

`documentSymbol` has no benchmark of its own. The walk is over a cached
`FileSyntax` that the completion and loop-latency benches already pay for — but
the walk is not its dominant cost: serialising a 20 000-method outline costs
more than the loop costs to reach, and nothing measures that. Worth a bench the
first time the outline feels slow.

## Size tiers

Inputs are generated, not committed — a checked-in 50k-line Java file is a
repository people have to clone, while a generator is twenty lines and always
reproducible. `javelin_benchmarks::Tier` defines:

| Tier | Methods | Stands for |
|---|---|---|
| `small` | 10 | A typical hand-written class |
| `medium` | 200 | A large service class |
| `large` | 2 000 | A generated client or a legacy god-class |
| `huge` | 20 000 | Pathological source; must degrade, never fall over |

Real-world projects belong in `tests/projects/` once workspace-level features
exist. They measure something the generator cannot: the messy shape of actual
code.

## Running benchmarks

```bash
scripts/bench.sh                # everything
scripts/bench.sh parsing        # one target
scripts/bench-startup.sh        # process startup (uses hyperfine if installed)
```

Criterion stores results under `target/criterion` and compares each run against
the previous one, so the useful workflow is:

```bash
git switch main   && scripts/bench.sh parsing   # establish the baseline
git switch mybranch && scripts/bench.sh parsing # read the change report
```

CI compiles the benchmarks but does not run them. Numbers from a shared runner
are noise, and a noisy gate gets ignored.

## Measured at the end of Phase 1

All of it was measured on one machine — Apple Silicon, darwin 25.6.0, release
build, otherwise idle. The parsing, edit and completion tables are runs at the
last Phase 1 commit; the per-stage breakdown, the loop-latency comparison and
the startup figures were taken during the tasks that produced them, at their
own commits, and are marked where that matters. Criterion medians where a
single number is given; percentiles come from the benches' own manual sample
loops.

These are a record of where the code stood, not a gate. Numbers from another
machine will differ, and only the ratios travel.

**Every number in this section was measured against the pre-3a fixture and is
not comparable with the section below it.** Phase 3a grew `java_source` so the
suite would exercise what Phase 2b and 2c added, which re-baselined all of it.
These rows stay because they are Phase 1's record; the current figures are
under [Measured at the end of Phase 3a](#measured-at-the-end-of-phase-3a).

### Parsing and edits

`cargo bench --bench parsing`, `cargo bench --bench documents`, both `--quick`,
at `f98de1f`.

This table measures `JavaParser` directly, not the server path. `ServerState`
reparses from the last *installed* tree, so an edit arriving before the previous
reparse has landed falls back to a full parse — the "keystroke, full reparse"
column, not the incremental one. Under sustained typing at `Tier::Huge` the pool
is always behind, so every reparse there is a full parse. The `ponytail:` comment
on `ServerState::did_change` names the upgrade.

| Tier | Parse from scratch | Keystroke, full reparse | Keystroke, incremental | Speed-up |
|---|---|---|---|---|
| `small` | 124 µs | 237 µs | 188 µs | 1.3× |
| `medium` | 2.48 ms | 2.49 ms | 667 µs | 3.7× |
| `large` | 25.9 ms | 25.4 ms | 5.20 ms | 4.9× |
| `huge` | 269 ms | 276 ms | 55.0 ms | 5.0× |

Against the < 10 ms first-parse budget: `small` and `medium` are inside it,
`large` and `huge` are not and never were — that is a `didOpen` on a 2 000- and
a 20 000-method generated class. Incremental reparsing does not change the first
parse; it changes what every keystroke *after* it costs.

Splicing the text and rebuilding the line table is a separate, smaller cost that
scales the same way: `document/keystroke` is 1.14 µs at `small` and 4.47 ms at
`huge`, because the buffer is rebuilt as a flat `String` per change. The
`ponytail:` comment on `TextDocument::with_changes_reporting` names the rope
upgrade.

### Completion

`cargo bench --bench completion`, at `f98de1f`. `warm` is the request against
views the server already holds; `after_edit` is the whole keystroke-to-answer path, timer started
before the keystroke. Both include serialising the response.

| | p50 | **p95** | p99 | max | budget |
|---|---|---|---|---|---|
| `warm`, `large` | 549 µs | **608 µs** | 727 µs | 814 µs | 50 ms ✅ |
| `warm`, `huge` | 4.13 ms | **9.90 ms** | 16.5 ms | 39.0 ms | 50 ms ✅ |
| `after_edit`, `large` | 31.2 ms | **35.3 ms** | 53.4 ms | 78.6 ms | 50 ms ✅ at p95 |
| `after_edit`, `huge` | 315 ms | **352 ms** | 405 ms | 433 ms | 50 ms ❌ |

**Completion misses its p95 budget at `Tier::Huge`: 352 ms against < 50 ms.**
Stated plainly because it is the one Phase 1 budget that is missed.

The `after_edit` / `Tier::Large` p99 figure of 53.4 ms was never reproduced. A
run of the same binary at `a12352c` printed 31.9 ms for it, which was recorded
here as pending re-measurement rather than as current; then the fixture
changed, so the old input is gone. On the Phase 3a fixture that row measures
42.5 ms — inside budget. It is retired as unreproduced rather than confirmed
or refuted, and it is not a miss today. Completion's `Tier::Huge` p95 miss
stands under every measurement anyone has taken of it.

The cost is not the query — `warm` is the query, and it is 5–80× inside budget.
It is that `ServerState` rebuilds `FileSyntax` and `ScopeTree` **whole** for
every document version. Measured per stage during a keystroke, when completion
landed:

| | text rebuild | incremental reparse | `FileSyntax::build` | `ScopeTree::build` | completion |
|---|---|---|---|---|---|
| `large` | 0.24 ms | 5.4 ms | 21.4 ms | 8.4 ms | 0.5 ms |
| `huge` | 3.2 ms | 63.7 ms | 216.6 ms | 101.5 ms | 4.0 ms |

Known, deferred, and deliberately not Phase 1's work: making those two rebuilds
incremental is its own piece of work. Phase 1 moved the cost off the request
loop (below) rather than removing it. `after_edit` waits for the rebuild on
purpose, so the number keeps meaning "what a keystroke costs" — in an editor
the request arriving meanwhile is answered from the previous view instead, one
version behind. Responsive, but not yet current.

`after_edit` at `Tier::Large` is inside budget at p95 and, on this run,
*outside* it at p99 (53.4 ms). By this document's own rule a slow p99 is a bug
even when p50 is fine, so it pointed at the same rebuild — but see the note
above: no later run reproduced it.

### Member completion

`cargo bench --bench completion`, at `a12352c`. `member_warm` and
`member_after_edit` ask the same two questions as `warm`/`after_edit` above, of
completion after a `.` instead of an unqualified prefix. The receiver is
`Receiver`, a two-member type the fixture declares purely so the dot has
something real to answer with; it is deliberately not tier-sized, so what those
two rows isolate is the cost of *finding* the receiver rather than of walking
it. `member_god_warm` is the other half: the receiver is `Generated` itself, so
the walk visits the tier's own 2 × N fields and methods. `this.` in a god class
and a field typed as one of the file's own large types are the ordinary shapes
that cost, and an earlier version of this document asserted they were bounded
rather than measuring them. They are measured now.

This adds no row to the budget table, but not because the walk is free:
completion's existing 50 ms row already covers it. Walking the receiver type's
members is bounded by that type and its file-local supertypes, not by the file
— true as of `f1a18d3`, and not true before it, when `members_of` resolved
every member's type eagerly and each of those resolutions walked the whole
file's declaration tree.

| | p50 | **p95** | p99 | max | budget |
|---|---|---|---|---|---|
| `member_warm`, `large` | 18.4 µs | **18.8 µs** | 19.1 µs | 24.8 µs | 50 ms ✅ |
| `member_warm`, `huge` | 479 µs | **655 µs** | 732 µs | 838 µs | 50 ms ✅ |
| `member_god_warm`, `large` | 226 µs | **234 µs** | 236 µs | 239 µs | 50 ms ✅ |
| `member_god_warm`, `huge` | 1.16 ms | **1.44 ms** | 1.56 ms | 1.70 ms | 50 ms ✅ |
| `member_after_edit`, `large` | 29.1 ms | **30.3 ms** | 31.0 ms | 33.0 ms | 50 ms ✅ |
| `member_after_edit`, `huge` | 317 ms | **325 ms** | 334 ms | 360 ms | 50 ms ❌ |

**`member_god_warm` meets the budget at both tiers**, at 1.44 ms p95 against
50 ms on a receiver with 40 000 members. It is the case the two-member
`Receiver` fixture cannot reach, and it does not add a miss.

`member_warm` is far under budget at both tiers, though by wildly different
margins — 18.8 µs against 50 ms at `large` and 655 µs at `huge`, which is
nearly three and a half orders of magnitude at one tier and under two at the
other, so no single figure ever covered both. It is not flat either: 18.8 µs to
655 µs is 35× against a 10× increase in tier size, and that growth is real
rather than noise. Member completion has two stages with
different cost profiles. Finding the receiver's own declaration is a scan:
`ScopeTree::scope_index_at` walks the sibling scopes of the enclosing type
looking for the one containing the cursor, and `FileSyntax::declaration_at`
walks declarations in source order looking for the matching name span — both
scale with how many members precede the cursor, which is exactly what `r`'s
position at the file's halfway mark charges for. `Receiver` has two members
regardless of tier, so `member_warm`'s growth is entirely that first stage.
`member_god_warm` pays the same first stage and adds the second: the 234 µs to
1.44 ms it costs over `member_warm` at the two tiers is the walk over 4 000 and
40 000 members.

`ScopeTree::scope_index_at` is linear in the enclosing type's own scope count,
because it descends from the root scanning each level's children. Its cost is
the "how many members precede the cursor" term above.
`FileSyntax::declaration_at` is linear in the **whole file's** declaration
count, not a type's: it searches depth-first from `FileSyntax::declarations`
and does not know which type it is looking inside. That is the reason
`javelin_semantic::member_type` resolves a member's declared type on demand
rather than for every member `members_of` returns — doing it eagerly made the
member walk O(members × file). Measured in a debug build on a 8 000-member
receiver whose members are typed as a class the same file declares, the eager
version took 380 ms and the lazy one 0.7 ms; that ratio is pinned by
`a_receiver_with_thousands_of_members_answers_without_walking_the_file_per_member`
in `crates/semantic/src/queries.rs`, because no benchmark here reaches it — the
generator types every member `int`, so resolving one in this fixture is a
keyword lookup. If the remaining scan ever shows up, the upgrade is to index
declarations by name span instead of walking them in source order.

**`member_after_edit` misses its p95 budget at `Tier::Huge`: 325 ms against
< 50 ms — joining the existing miss named above.** This is not a cost the
member query introduces: `member_after_edit` pays for the same whole-document
`FileSyntax`/`ScopeTree` rebuild that `after_edit` already pays for (325 ms at
this tier, within noise of the number above), before either one runs its own
query. The member query adds under a millisecond on top of that rebuild, which
`member_warm` already showed. Both numbers are named here rather than one
quietly standing in for the other, so a future change that fixes the rebuild
and a future change that regresses the member query itself cannot hide behind
each other.

### Request latency while a reparse runs

`cargo bench --bench loop_latency`; the "after" column at `6209584`, the
"before" at `5c3a566`, the confirmation re-run at `f98de1f`. One iteration is a
`didChange` on a
`Tier::Huge` buffer (20 000 methods, ~2.4 MB) followed immediately by a
`textDocument/hover`, timed over a real `serve` loop and a real in-memory
connection.

| | before the parse pool | after |
|---|---|---|
| p50 | 338 ms | 6.0 ms |
| p95 | 388 ms | 13.9 ms |
| p99 | 436 ms | 23.1 ms |
| max | 793 ms | 47.6 ms |

The "before" column was measured by running the same bench in a `git worktree`
at the commit preceding the pool, so both columns are the same code measuring
different servers. Both were taken when the pool landed; a re-run at the final
commit lands in the same range rather than on the same numbers (p50 5.5 ms, p95
8.0 ms, p99 14.2 ms — a 1.7× spread at the tail, which is what ten Criterion
samples of a 300 ms reparse buys you).

This is the whole point of the pool: before, the hover waited behind a reparse
the loop had to finish first. After, it is answered from the previous coherent
view while the reparse runs on a worker. The remaining ~6 ms at p50 is not the
pool — it is `didChange` splicing and re-indexing a 2.4 MB string, which still
happens on the loop.

### Startup

`scripts/bench-startup.sh` and `cargo bench --bench startup`.

| | |
|---|---|
| Process startup, mean over 100 runs of `javelin --version` | 8 ms (budget < 50 ms ✅) |
| `ServerState::new` | 148 µs — mostly spawning the pool's threads |
| `initialize` round trip, p95 | 9.2–14.5 µs (budget < 100 ms ✅) |

`Connection::initialize` puts the capabilities response on the wire before
`serve` constructs the state, so the pool cannot delay the handshake even in
principle.

### Not measured here

Editor-observed latency. Benchmarks and a real client answer different
questions, and the editor pass in
[`manual-testing-zed.md`](manual-testing-zed.md) is where the second one gets
answered — walked by hand for the phases named below, automated since
2026-09-11, with the automated run's own numbers in
[`editor-pass-results.md`](editor-pass-results.md) (a debug build, so not
comparable with anything in this file). Phases 1, 2a and 2b were all walked on
2026-09-03 and Phase 2c on 2026-09-04; the measured startup, first-parse and
reparse numbers are in that file's results table, as is the one bug the pass
caught that no benchmark could. 2c's walk found something of the same kind,
though about the measurement rather than the code: the startup column also counts the time the
client spends before it sends `initialize`, so it is not comparable across rows
taken under different client conditions. Javelin's own share is 1–3 ms.

Phase 3a was walked twice: over real pipes on 2026-09-07, and in Zed on
2026-09-08, which is the walk that signed it off. Its row carries both numbers
the cache exists to contrast — `classpath ready` after 13 043 ms of `src.zip`
on a cold session, and 43 ms from the cache on a warm one, 24 467 facts either
way. The benchmark's own figures for the same two paths are 11 175 ms and
15–38 ms; the editor's warm number is three times the bench's because it is a
whole process start rather than a timed read, and both are tens of milliseconds
against eleven seconds, which is the only comparison that matters here.

## Measured at the end of Phase 3a

Same machine as the tables above — Apple Silicon, darwin 25.6.0, release build,
otherwise idle — and one full run of `scripts/bench.sh`, not a controlled
multi-run average. Percentiles come from each bench's own `--bench`-gated
200-sample loop; a single figure is Criterion's median.

**The twice-deferred fixture debt is closed, and this section is what closing
it cost.** `java_source` in [`benchmarks/src/lib.rs`](../benchmarks/src/lib.rs)
now emits, per generated method, a `var` local with an initialiser and an
unqualified call to the next generated method, plus one overload set per tier
(`over(int)`, `over(Generated)`, `over(String)`) with a call site for each. So
the suite finally exercises what Phase 2b and 2c added. Phase 2b deferred this
and Phase 2c deferred it again, both times because closing it re-baselines
every number in all three phases' tables at once — which is exactly what
happened, and why the section above is now labelled as a record rather than as
current.

The fixture grew 26–39%: `small` 2029 → 2811 bytes, `medium` 40909 → 53001,
`large` 418509 → 539601, `huge` 4284509 → 5513601. It is not purely additive:
inside the pre-existing `if`, `return local * 2;` became `return doubled;`, so
the new `var` local has a use rather than sitting dead. Nothing searched for
the old text.

### Parsing and edits

| Tier | Parse from scratch | Keystroke, full reparse | Keystroke, incremental | Speed-up |
|---|---|---|---|---|
| `small` | 180.66 µs | 181.45 µs | 32.769 µs | 5.5× |
| `medium` | 3.1818 ms | 3.2087 ms | 447.54 µs | 7.2× |
| `large` | 32.774 ms | 33.019 ms | 4.4680 ms | 7.4× |
| `huge` | 340.30 ms | 348.09 ms | 55.507 ms | 6.3× |

Against the < 10 ms first-parse budget: `small` and `medium` inside it,
`large` and `huge` outside it — the same verdict as Phase 1 at bigger numbers,
not a new miss.

**Do not read the `small` speed-up as an improvement.** It was ~1.3× before
this fixture change and is 5.5× after it. `documents.rs` edits at
`text.len() / 2`, unchanged code, and that offset now lands on different
content, so what moved is where the edit falls and not how well Tree-sitter
reuses subtrees.

| Tier | `document/open` | `document/keystroke` (splice) |
|---|---|---|
| `small` | 1.4314 µs | 1.5160 µs |
| `medium` | 20.601 µs | 22.352 µs |
| `large` | 186.27 µs | 203.13 µs |
| `huge` | 1.9037 ms | 2.3893 ms |

`document/position_at_end_of_large_file`: 12.032 ns, unaffected by the fixture
because it measures the end of the buffer whatever is in it.

### Startup and `initialize`

| | median |
|---|---|
| `parser/new` | 275.45 ns |
| `startup/engine` (`ServerState::new`) | 121.04 µs |
| `startup/initialize`, Criterion | 25.134 µs |

| | p50 | **p95** | p99 | max | budget |
|---|---|---|---|---|---|
| `initialize` | 8.25 µs | **14.542 µs** | 17.458 µs | 23.875 µs | 100 ms ✅ |

Process startup, `scripts/bench-startup.sh` over 100 runs of
`javelin --version`: **10 ms**, against 11 ms for a release binary built from
the parent commit `e8da3ea` — no measurable cost, and the budget is 50 ms. The
binary grew 3.37 MB → 5.62 MB when the server picked up `javelin-index` and
its bundled SQLite, and none of that is paid at startup because nothing on the
startup path opens the cache or the zip.

### Completion

Unchanged in shape from Phase 1; only the fixture under it moved.

| | p50 | **p95** | p99 | max | budget |
|---|---|---|---|---|---|
| `warm`, `large` | 586.083 µs | **615.166 µs** | 638.292 µs | 655.958 µs | 50 ms ✅ |
| `warm`, `huge` | 6.234 ms | **6.663 ms** | 6.893 ms | 7.100 ms | 50 ms ✅ |
| `after_edit`, `large` | 38.579 ms | **39.597 ms** | 42.526 ms | 44.982 ms | 50 ms ✅ |
| `after_edit`, `huge` | 410.199 ms | **417.875 ms** | 425.246 ms | 453.702 ms | 50 ms ❌ |
| `member_warm`, `large` | 21.958 µs | **23.375 µs** | 28.292 µs | 28.875 µs | 50 ms ✅ |
| `member_warm`, `huge` | 629.208 µs | **854.75 µs** | 953.459 µs | 1.158 ms | 50 ms ✅ |
| `member_god_warm`, `large` | 239.25 µs | **247.75 µs** | 256.917 µs | 263.833 µs | 50 ms ✅ |
| `member_god_warm`, `huge` | 1.3975 ms | **1.7069 ms** | 1.8477 ms | 1.8969 ms | 50 ms ✅ |
| `member_after_edit`, `large` | 38.947 ms | **40.823 ms** | 43.297 ms | 59.935 ms | 50 ms ✅ |
| `member_after_edit`, `huge` | 414.654 ms | **420.976 ms** | 440.715 ms | 449.923 ms | 50 ms ❌ |

The two misses are the two `after_edit` rows at `Tier::Huge`, and they are one
cost with two names: the whole-document `FileSyntax` and `ScopeTree` rebuild,
which both rows wait for on purpose. The queries themselves are the `warm`
rows, 81× inside budget at `Tier::Large` and 7.5× at `Tier::Huge`.

### Definition — new in Phase 3a

`benchmarks/benches/definition.rs`, through
`javelin_server::handlers::definition::handle` with a real `ServerState` and
real JSON serialisation. Three shapes: `local` and `field` go through
`javelin_parser::definition_at`, and `call` goes through
`javelin_semantic::call_definition_at` on its ordinary case, where exactly one
method of that name fits. Each shape asserts once, outside the timed loop,
that the position still resolves — a benchmark of a `None` would be fast and
meaningless.

| | p50 | **p95** | p99 | max | budget |
|---|---|---|---|---|---|
| `local`, `small` | 2.500 µs | **2.583 µs** | 2.667 µs | 2.917 µs | 50 ms ✅ |
| `field`, `small` | 2.542 µs | **2.625 µs** | 2.708 µs | 3.167 µs | 50 ms ✅ |
| `call`, `small` | 5.250 µs | **6.875 µs** | 31.292 µs | 35.459 µs | 50 ms ✅ |
| `local`, `medium` | 6.584 µs | **6.792 µs** | 7.917 µs | 24.250 µs | 50 ms ✅ |
| `field`, `medium` | 6.584 µs | **8.125 µs** | 8.750 µs | 9.000 µs | 50 ms ✅ |
| `call`, `medium` | 5.708 µs | **5.958 µs** | 7.292 µs | 7.334 µs | 50 ms ✅ |
| `local`, `large` | 45.709 µs | **50.000 µs** | 56.084 µs | 56.834 µs | 50 ms ✅ |
| `field`, `large` | 47.833 µs | **52.542 µs** | 59.167 µs | 67.084 µs | 50 ms ✅ |
| `call`, `large` | 33.083 µs | **34.542 µs** | 37.125 µs | 41.292 µs | 50 ms ✅ |
| `local`, `huge` | 1.9172 ms | **2.2131 ms** | 2.3073 ms | 2.3684 ms | 50 ms ✅ |
| `field`, `huge` | 2.0554 ms | **2.3395 ms** | 2.4106 ms | 2.4562 ms | 50 ms ✅ |
| `call`, `huge` | 332.917 µs | **392.292 µs** | 462.167 µs | 472.542 µs | 50 ms ✅ |

**Definition meets its 50 ms p95 budget at every tier and every shape**, worst
case 2.34 ms at `Tier::Huge`, ~21× under. That row in the budget table stops
being "neither met nor missed" after three phases of being exactly that.

### Argument-type elimination — new in Phase 3a

`benchmarks/benches/overload_elimination.rs`, through
`javelin_semantic::member_definition_at` on `this.over(1)`, so
`sole_candidate` walks `members_of(Generated)` twice — once per namespace —
over the tier's own thousands of members before either exclusion runs. Two
variants: `no_externals`, which every other bench in the crate runs under, and
`hand_built_classpath`, a bench-local `Externals` over three fixed facts
(`java.lang.String`, `java.lang.Integer`, `java.lang.Object`). No `TypeWorld`
and no real JDK read, which would cost eleven seconds and vary per machine.

| | p50 | **p95** | p99 | max |
|---|---|---|---|---|
| `no_externals`, `small` | 2.458 µs | **2.583 µs** | 3.125 µs | 10.583 µs |
| `hand_built_classpath`, `small` | 2.708 µs | **3.333 µs** | 3.542 µs | 4.083 µs |
| `no_externals`, `medium` | 5.750 µs | **5.875 µs** | 5.917 µs | 12.458 µs |
| `hand_built_classpath`, `medium` | 6.583 µs | **6.792 µs** | 8.250 µs | 9.458 µs |
| `no_externals`, `large` | 38.375 µs | **39.667 µs** | 42.916 µs | 98.708 µs |
| `hand_built_classpath`, `large` | 39.000 µs | **44.000 µs** | 48.375 µs | 50.875 µs |
| `no_externals`, `huge` | 466.666 µs | **995.333 µs** | 1.1627 ms | 1.3923 ms |
| `hand_built_classpath`, `huge` | 397.959 µs | **517.583 µs** | 567.583 µs | 584.958 µs |

Both variants sit well inside definition's budget at every tier — worst case
~1.0 ms at `Tier::Huge`, ~50× under. At that tier `no_externals` runs *higher*
than the classpath variant (995 µs against 518 µs p95), which is the direction
the bench was built to show rather than an anomaly: without a classpath
`over(String)` cannot be excluded, both candidates survive, and refusing still
means every candidate was checked and none settled the question. With one,
`int` boxes to `java.lang.Integer`, `over(String)` goes, and `over(int)` is the
sole survivor. It is not the own-shadows-inherited walk buying that either —
both survivors are declared directly on the generated class, so
`hides_the_rest`'s `matches.len() > 1` branch never fires in either variant.
This is one run, so the direction travels and the ratio does not.

### Request latency while a reparse runs

`loop_latency.rs`, unchanged in shape. Criterion median 6.8138 ms; the manual
200-sample loop reports p50 5.789 ms, p95 11.822 ms, p99 21.203 ms, max
76.387 ms, against a reparse it does not wait for of ~300 ms. Same conclusion
as when the pool landed: the request is answered from the previous coherent
view instead of queueing behind a file-sized reparse.

### Reading the JDK

The one-off cost the classpath exists to amortise, measured by
`reading_a_real_jdk_reports_its_cost` in `crates/classpath/src/sources.rs` —
`#[ignore]`d, because it reads whatever JDK the machine has. Temurin 25,
release build, `src.zip` 53 646 286 bytes with 15 371 zip entries:

| | |
|---|---|
| Type facts produced | 24 467 |
| Elapsed, single-threaded | **10.95 s** (10.77 s and 10.95 s on two more runs) |
| Peak RSS of the whole test process | ~76 MB (`/usr/bin/time -l`) |

Almost all of that is Tree-sitter parsing 15 371 files one at a time. Neither
the zip I/O nor the name-resolution pass is the cost, and parallelising the
per-entry parse is the obvious lever if it ever needs pulling.

The SQLite round trip that makes it a one-off, at 40 000 facts — ~1.6× a real
JDK's count: **~0.10 s** in release, ~0.15 s in dev. Two orders of magnitude
cheaper than the parse it replaces.

End to end, release binary over real pipes with a Temurin 25 on the machine,
timed from the `initialized` notification to `Readiness::Dependencies`:

| | `initialized` → `Readiness::Dependencies` | facts | source |
|---|---|---|---|
| Cold, empty cache | **11 175 ms** | 24 467 | `src.zip` |
| Warm, same cache | **15–38 ms** | 24 467 | `cache` |

The warm figure is what the cache buys, against the "warm startup in hundreds
of milliseconds" target, and it is met comfortably: 38 ms at its worst against
a target measured in hundreds.
The cold figure has no budget because nothing waits on it: every feature
answers from the open buffer throughout, which is Phase 2's answer, and the
classpath's contribution arrives when it arrives. It is still the largest
single cost in the product, and it is stated here rather than buried in the
warm number.

### `FileSyntax::declares_type_named`

The file has the last word over the classpath, so every name the classpath
resolves is checked against "does this file declare a type of that name?" —
which puts the lookup on the path of every external name in the buffer. The
first implementation was a recursive walk of the declaration tree, measured at
**173 µs** per call at 40 001 top-level declarations. It is now a binary search
over a sorted vector of name hashes: **~19 ns**, roughly 9000× faster, costing
**320 008 bytes** per `FileSyntax` at that declaration count — against ~1.5 MB
for the `HashSet<String>` that would remove the collision caveat. Both figures
come from throwaway `#[ignore]`d tests, run in release at 100 000 iterations
per case and not committed; nothing in the Criterion suite covers this.

A hash collision answers `true` for a name the file does not declare. Every
caller uses the method to *veto* an answer built from the classpath, so a
collision costs a missing answer and never a wrong one — the safe direction,
named in a `ponytail:` comment next to the field.

### Phase 1's targets, re-measured

| Miss recorded before Phase 3a | Old number | New number | Verdict |
|---|---|---|---|
| Completion p95, `Tier::Huge` | 352 ms | 417.875 ms | Still missed, bigger, same miss |
| Completion p99, `Tier::Large` | 53.4 ms | 42.526 ms | **No longer missed**, and never reproduced |
| Completion after a `.`, `Tier::Huge` p95 | 326 ms | 420.976 ms | Still missed, bigger, same miss |
| First parse, `Tier::Large` | 25.9 ms | 32.774 ms | Still missed, bigger, same miss |
| First parse, `Tier::Huge` | 269 ms | 340.30 ms | Still missed, bigger, same miss |

**No previously-met target is newly missed.** The four surviving misses are
each larger, which is the grown fixture and not a regression, and every one of
them is the same row it was before. Definition, new to the suite, meets its
budget everywhere.

### Still not measured

- **Local references.** No benchmark, so its 100 ms row is neither met nor
  missed. It will get one when the feature exists, which is Phase 3b.
- **`documentSymbol`.** Still no benchmark of its own; the walk is over a
  cached `FileSyntax` the completion and loop-latency benches already pay for,
  and serialising a 20 000-method outline is the part nobody measures.
- **Memory, under an allocator profiler.** The ~76 MB above is
  `/usr/bin/time -l`'s resident-set figure for a whole test process, not a heap
  profile. Nothing here measures the steady-state cost of holding a
  `TypeWorld` for 24 467 types.
- **Editor-observed latency**, which is the editor pass's job rather than this
  file's. Phase 3a's row in
  [`manual-testing-zed.md`](manual-testing-zed.md) was filled on the Zed walk
  of 2026-09-08, and Phase 3b's is not owed until the phase ends. The automated
  pass records its own numbers in
  [`editor-pass-results.md`](editor-pass-results.md), deliberately coarse and
  from a debug build.

## Re-measured on Phase 3b's addressing slice

Two benches re-run on 2026-09-09 against the addressing slice
(`Span` becoming `(FileId, Span)` throughout `javelin-semantic`), because that
change touches the crate's hottest loop twice: `DocumentId` equality is an
`Arc<str>` *content* comparison with no pointer fast path, so every `Files`
lookup and every `seen.contains(&site)` frame comparison compares a URI where
it previously compared four `u32`s, and `members_of` clones a `FileId` per
emitted member. No answer changed, so **the tables above are not re-baselined**
— this section records what the re-run measured and nothing else.

`cargo bench --bench completion` and `cargo bench --bench overload_elimination`,
one run each, on the same machine as the tables above but *not* an idle one:
an editor and a JDTLS process were up throughout. p95, against the Phase 3a
figures for the same rows:

| Row | Phase 3a p50 / p95 | This run p50 / p95 |
|---|---|---|
| `completion warm`, `huge` | 6.234 / 6.663 ms | 5.771 / 6.372 ms |
| `completion after_edit`, `huge` | 410.199 / 417.875 ms | 410.564 / 479.457 ms |
| `member_warm`, `huge` | 629.208 / 854.75 µs | 573.625 / 836.625 µs |
| `member_god_warm`, `huge` | 1.3975 / 1.7069 ms | 1.5165 / 1.79625 ms |
| `member_after_edit`, `huge` | 414.654 / 420.976 ms | 407.867 / 456.296 ms |
| `overload no_externals`, `huge` | 466.666 / 995.333 µs | 471.167 / 593.542 µs |
| `overload hand_built_classpath`, `huge` | 397.959 / 517.583 µs | 459.5 / 548.375 µs |

**Nothing here separates a cost from the noise, and the reason is worth
stating.** Every p50 is within a few per cent of Phase 3a's and four of the
seven are *lower*. The two `after_edit` rows' p95 tails widened by 35–60 ms
while their p50s held to within half a per cent — and those rows measure a
whole-document `FileSyntax`/`ScopeTree` rebuild that this branch does not
touch, so that is a busy machine's tail rather than added work. The two rows
that would show a per-member cost most directly moved in opposite directions:
`member_warm` at `Tier::Huge` came in 18 µs *below* Phase 3a's p95 and
`member_god_warm` 89 µs above it. `overload hand_built_classpath` is the row
most worth watching — it walks `members_of(Generated)` twice over the tier's
thousands of members, which is where a per-member clone would show — and its
p50 is up 15%, but its p95 sits 31 µs above a Phase 3a figure whose own
`no_externals` sibling was 995 µs and is now 594 µs. One run on a machine with
an editor on it cannot tell 15% from that.

So: no regression is claimed and none is ruled out, and the two misses in the
tables above are the same two rows they have been since Phase 1. What would
settle it is the same bench on an idle machine, which is worth doing when a
`Files` set really holds two files rather than now, because that is when the
cost stops being hypothetical: the upgrade is a dense `u32` file id with the
URI held once, confined to `files.rs` and its boundaries. `DeclSite`'s field
order is the free half — declared `{ file, span }`, its derived `PartialEq`
compares the URI first, where `{ span, file }` would decide the overwhelmingly
common mismatch on integers. Neither is worth doing against a number this run
cannot establish.

## Measured on Phase 3b's external-members slice

Measured on 2026-09-09, same machine as every table above, release build. Not a
dedicated benchmark machine: two idle JDTLS instances and a Gradle daemon were
resident throughout, though the system sat at ~86% idle while the benches ran.
One run of each bench, not a controlled multi-run average. Percentiles come
from each bench's own `--bench`-gated 200-sample loop; a single figure is
Criterion's median.

This slice touched three things a number can see: it made `read_type_facts`
produce a `MemberFacts` per member each type declares, it put those in the
fact cache, and it gave the request path a third question to ask the world.
**No budget's status changed.** The four rows this document records as missed
are the same four: completion's p95 and completion-after-a-`.`'s p95 at
`Tier::Huge`, which are the two `after_edit` rows in the table below, and first
parse at `Tier::Large` and `Tier::Huge`, which is a `didOpen` cost no bench in
this slice touches. Neither `after_edit` row is a new miss and neither miss is
this slice's work — the completion one goes back to Phase 1, while
`member_after_edit` measures a feature that did not exist until Phase 2a. And
the new row meets its budget by 41× at its worse tier. What did move is the
cold read, the cache on disk and the warm read, and the last of those moved
by ~4×.

### Reading the JDK, re-measured

`reading_a_real_jdk_reports_its_cost` in `crates/classpath/src/sources.rs`,
`#[ignore]`d and run by hand, release build, three runs. Same machine and same
JDK as Phase 3a's row — a JDK 25 whose `src.zip` is 53 646 286 bytes with
15 371 entries:

| | Phase 3a | This slice |
|---|---|---|
| Type facts produced | 24 467 | 24 467 |
| Member facts produced | none stored | **208 675** |
| Elapsed, single-threaded | 10.95 s (10.77 s, 10.95 s) | **11.41 s, 11.07 s, 11.03 s** |

**Producing 208 675 member facts cost between 1% and 4%**, and that is the
finding rather than a surprise survived. Phase 3a already established that
almost all of the eleven seconds is Tree-sitter parsing 15 371 files one at a
time; the members are read off trees that pass had already built, and their
type names go through the same second pass that was already resolving supertype
names. The growth is **real and small, not lost in the noise**: the two run
sets do not overlap, since the fastest new run (11.03 s) is slower than the
slowest 3a run (10.95 s), so no new sample is consistent with 3a's cost. Nor is
it inside either set's own spread — 3a's three runs spanned 180 ms and these
three span 380 ms, while the largest growth is 460 ms. Six samples on a shared
machine cannot put a number on it more finely than the 1–4% bracket above, but
they are enough to say the read got slower. It is still "about eleven seconds".

### The cache, and the warm read

Measured over real pipes against the release binary, driven by a script that
sends `initialize`, then `initialized`, and reads the server's own
`classpath ready` log line — `duration_ms` and `source` — off stderr. That is
the same method Phase 3a's end-to-end row used, and it is stated because it is
the only way to get this number without a manual editor session: nothing in
the Criterion suite opens the cache, `javelin doctor` reports the cache's path
and fact count but never times a read, and `install_classpath` is
`pub(crate)`, so no in-process benchmark can install a world either. The cache
file was deleted first, so the cold run below is genuinely cold.

| | Phase 3a | This slice |
|---|---|---|
| Cold, empty cache | 11 175 ms | **11 334 ms** |
| `jdk.sqlite` on disk afterwards | 5.7 MB | **27.7 MB** |
| Warm, same cache | 15–38 ms | **103–139 ms** |

The times are `duration_ms` on `classpath ready`; the cold run's wall clock from
`initialized` to the log line was 11 640 ms, and the cache file is 27 709 440
bytes. The warm figure is six consecutive runs: 103, 105, 110, 110, 120, 139.

**The warm read is the number that moved: ~4× on the time, and 4.86× on the
file** — 5.7 MB to 27.7 MB is nearer five times than four, and it is the larger
of the two multiples rather than the same one.
It is still met against the "warm startup in hundreds of milliseconds" target,
and it is still not on any request's path: the read starts from the
`initialized` notification on a detached thread, and every feature answers from
the open buffer until it lands. The six runs were back-to-back, so the file was
in the OS page cache throughout — a first warm read after a reboot is not
measured here and would be slower.

The ~22 MB the cache grew is 208 675 member rows at roughly 105 bytes each: a
name, a `DeclKind` code, a staticness flag, a declared type, a
`params` column and a variadic flag, keyed `(fqn, position)`. What that bought
is the alternative not taken. Member types are resolved **at read time**, as
far as they resolve — a type variable, an array or an ambiguous name stays
`MemberType::Unresolved` — so a row is self-contained and the request path
reads facts and resolves nothing. Storing each member's type as the author wrote it
would have made the rows smaller, and would have required every declaring
file's package and import list in the cache as well — and then name resolution
would run on the request loop, once per member of every receiver, against the
rule this project keeps in exactly one place. Cheaper on disk, and the wrong
side of two of the file's own rules.

### Completion, with the new external row

`member_external_warm` is new: completion after a `.` on a receiver typed from
a bench-local `Externals` holding one synthetic type with **4 096 members**,
fixed rather than tiered. It is the only case in `completion.rs` that does not
go through `javelin_server::handlers::completion` — `install_classpath` is
`pub(crate)` and a real JDK read would cost eleven seconds and vary per
machine — so it calls `javelin_semantic::member_completions_at` directly, the
way `overload_elimination.rs` does. **It pays no handler and no JSON
serialisation, so it is not comparable with `member_warm` row for row.**

| | p50 | **p95** | p99 | max | budget |
|---|---|---|---|---|---|
| `member_external_warm`, `large` | 413.875 µs | **422.5 µs** | 436 µs | 548.333 µs | 50 ms ✅ |
| `member_external_warm`, `huge` | 1.0403 ms | **1.2299 ms** | 1.3151 ms | 1.5535 ms | 50 ms ✅ |

Two readings, and the fixed member count is what makes both available. The
`large`-to-`huge` gap is 626 µs on an identical member list, so it is the file
scan that finds the receiver's declaration and nothing else — and
`member_warm`, whose receiver has two members, moves 576 µs across the same
two tiers, which is the same file work measured independently. At `large`,
then, roughly 390 of the 414 µs is the 4 096-member list itself: **~96 ns per
external member.** `member_god_warm` walks 4 000 *file-local* members at
`large` in 245 µs while also paying the handler and serialising 500 items.
Dividing that whole 245 µs gives ~61 ns each and makes the external member look
only half again as expensive — but that compares a figure with the fixed cost
taken out against one with it left in, so **half again is a lower bound and
not the comparison.** Subtracting on both sides needs a fixed cost for the
local walk, and the only one available is the `F ≈ 110 µs` the two-point fit
below gives: that puts the local member at ~34 ns and the ratio nearer **2.8×**.
The fit is not established — it is hedged where it is stated — so read the
range 1.5×–2.8× rather than either end of it.

That per-member figure is worth one caveat, because it is easy to over-read.
Most of it is materialising the list rather than walking anything: an
`ExternalMember` carries two `String`s and a `Vec`, `Externals::members`
answers by value, and the bench-local world clones its vector per call. The
real `ClasspathExternals::members` allocates those strings per call too, so
the shape is right — but it also walks a supertype closure, which the
bench-local world has none of. **Nothing here measures the world's own closure
walk**, and it is the part that would grow with a deep hierarchy.

The rest of the suite, against Phase 3a and against the addressing slice, p50 /
p95 at `Tier::Huge`:

| Row | Phase 3a | Addressing slice | This run |
|---|---|---|---|
| `completion warm` | 6.234 / 6.663 ms | 5.771 / 6.372 ms | 5.910 / 6.367 ms |
| `completion after_edit` | 410.199 / 417.875 ms | 410.564 / 479.457 ms | 399.236 / 410.205 ms |
| `member_warm` | 629.208 / 854.75 µs | 573.625 / 836.625 µs | 597.083 / 765.083 µs |
| `member_god_warm` | 1.3975 / 1.7069 ms | 1.5165 / 1.79625 ms | 1.6815 / 1.9932 ms |
| `member_after_edit` | 414.654 / 420.976 ms | 407.867 / 456.296 ms | 401.897 / 410.473 ms |
| `overload no_externals` | 466.666 / 995.333 µs | 471.167 / 593.542 µs | 444.708 / 557.417 µs |
| `overload hand_built_classpath` | 397.959 / 517.583 µs | 459.5 / 548.375 µs | 439.25 / 463.917 µs |

**Six of the seven rows are quieter than the addressing slice's run and inside
a few per cent of Phase 3a's, and one is not.** `member_god_warm` at
`Tier::Huge` has now risen in all three runs — p50 1.3975 → 1.5165 → 1.6815 ms,
+20% end to end — and it is the row where a per-member cost on the *file-local*
walk would show, since it walks the tier's own 40 000 members. Two changes
could produce it: the `FileId` clone per emitted member from the addressing
slice, and the wholeness bookkeeping `walk_members` added in this one.

**It is still not established, and the reason is in the same table's other
tier.** The identical row at `Tier::Large` walks 4 000 members and moved
239.25 → 245.167 µs p50, **+2.5%** — and the two tiers can be compared properly
rather than by percentage, because two points fit the walk's shape. Taking
Phase 3a's own figures, `F + n·c` through (4 000, 239.25 µs) and
(40 000, 1.3975 ms) gives a fixed cost of **F ≈ 110 µs** and a per-member cost
of **c ≈ 32 ns**. The fixed share is therefore much larger at `large` than at
`huge`, so an equal *per-member* delta does not move both tiers by the same
percentage — it moves `huge` by about **1.7×** the percentage it moves `large`.
The `large` delta of 5.9 µs is 1.5 ns per member; at 40 000 members that
predicts ~59 µs, or **+4%** at `huge`. The observed move is +284 µs, +20%: a
**~5-fold** disagreement, which points at memory behaviour at 40 000 members or
at run-to-run drift rather than at added work per member. Both figures are
25× or more inside the 50 ms budget either way. What would settle it is the
one thing not done here: several runs of `member_god_warm` alone, on a machine
with nothing else resident, against a binary built from `master`. That is
worth doing when the row approaches its budget, and it is nowhere near it.

`overload hand_built_classpath` — the row the addressing slice named as the one
most worth watching, because it walks `members_of(Generated)` twice — came in
*below* that run at both p50 and p95 and remains 10% above Phase 3a's p50 with
a p95 54 µs under it. Same verdict as before: no regression claimed, none ruled
out, and one run cannot tell 10% from a machine.

### Still not measured, updated

- **Local references.** Still no benchmark, because still no feature. It is
  what remains of Phase 3b along with the workspace source index, and this
  slice brought neither.
- **The world's supertype closure walk.** `ClasspathExternals::members` closes
  over the graph within `TypeWorld::MAX_VISITS`, and neither benchmark exercises
  it: `overload_elimination`'s world has three flat facts and
  `completion`'s new one has a single type with no supertypes. A deep real
  hierarchy — `java.util.ArrayList` and up — is where that cost lives.
- **Memory.** Unchanged as a gap and larger as a question: nothing profiles the
  steady-state cost of holding a `TypeWorld` over 24 467 types **and 208 675
  members**. Phase 3a's ~76 MB was `/usr/bin/time -l`'s resident set for a whole
  test process and is not a heap figure for either shape.
- **A warm read from cold page cache.** The 103–139 ms above is six
  back-to-back runs against a file the OS had just written and read.
- **Editor-observed latency.** Phase 3b's row in
  [`manual-testing-zed.md`](manual-testing-zed.md) is still owed and was
  deliberately not walked for this slice: the phase's pass belongs at the end
  of the phase. The 103–139 ms warm figure is what a walker should expect where
  3a's row records 43 ms.

## Measured on Phase 3b's definition slice

Measured on 2026-09-11, same machine as every table above, release build, and
this time an idle one — no editor, no JDTLS, no Gradle daemon. One run of each
bench, not a controlled multi-run average. Percentiles come from each bench's
own `--bench`-gated 200-sample loop; a single figure is Criterion's median.

This slice touched three things a number can see. It put provenance in the
facts — the `src.zip` entry per type and two byte offsets per member — and so
in the fact cache, which is schema 5 now. It gave `Definition` a `DocumentId`,
which `javelin-semantic` clones once per answered definition. And it put an
archive open and a file extraction on the request loop, for the first jump of a
session into the JDK's sources, which is the only genuinely *new* cost here.

**No budget's status changed.** The four rows this document records as missed
are the same four — completion's p95 and completion-after-a-`.`'s p95 at
`Tier::Huge`, and first parse at `Tier::Large` and `Tier::Huge` — and none of
them is touched by anything in this slice. The two new rows meet definition's
50 ms budget, cold included, and the section below says by how much and where
the cold number goes, because 2× is a narrower margin than anything else in
this file.

### The new rows: a jump into the JDK's sources

`benchmarks/benches/definition.rs`, group `definition_external`, over a
synthetic `src.zip` of 15 371 entries — the entry count of the Temurin 25
`src.zip` every other JDK figure here came from. `cold` is the first jump of a
session into one file: open the archive, read the entry under the config's
size cap, write it through a temporary name and rename, and convert the
member's byte offsets to a UTF-16 range. `warm` is every jump after that into
the same file: read what is on disk and convert.

Neither goes through `javelin_server::handlers::definition`, for the reason
`completion.rs`'s `member_external_warm` does not go through its own handler —
`install_classpath` is `pub(crate)`, `JdkSources` is crate-private, and a real
JDK read costs eleven seconds and varies by machine. So the bench runs the same
sequence `crates/server/src/jdk_sources.rs` runs, written out. **It pays no
handler, no member resolution and no JSON, so it is not comparable row for row
with `local`, `field` and `call` above**; it is the file work a jump into the
JDK *adds* to what those already cost.

| | p50 | **p95** | p99 | max | budget |
|---|---|---|---|---|---|
| `definition_external cold` | 23.872 ms | **25.259 ms** | 25.666 ms | 27.872 ms | 50 ms ✅ |
| `definition_external warm` | 39.333 µs | **46.292 µs** | 51.208 µs | 67.583 µs | 50 ms ✅ |

Criterion's medians for the same two rows: 24.093 ms and 38.313 µs.

**Cold meets the budget with about 2× of headroom, where every other definition
row has 20× or more, and essentially all of it is one thing: opening the
archive.** A throwaway third row, run once and not committed, measured
`SourceArchive::open` alone at **23.9 ms** against the synthetic archive — 99%
of the cold row — and **23.6 ms** against the machine's own
`lib/src.zip`. Those two figures agree to within 1.2%, which is the fixture's
realism established rather than asserted: a `src.zip` is opened by parsing its
central directory, the synthetic one has a real one's *entry count*, and its
being a few MB rather than 53 does not enter.

That cost is paid once per session and not once per jump — the archive handle
is opened on the first miss and kept, which was a deliberate
choice. So the shape
a user sees is one jump at ~24 ms and every later jump into the same file at
~39 µs, with a jump into a *different* file costing the read, the write and the
conversion — the 0.17 ms the cold row has left once the open is taken out.
Ruling 7 also said what to do with this number: record it. It is recorded, and
it is inside the budget, so nothing is reshaped. If it ever needs to move, the
lever is to open the archive when the classpath lands, on the thread that is
already there, which is a server change and not a benchmark one.

Two caveats, and the first is about the row with the least headroom. **The
cold row is cold in its handle and warm in its bytes.** Each iteration opens
the archive afresh, which is the cost being measured, but the fixture was
written moments earlier and opened twice before the first sample, so the
central directory those opens parse is in the OS page cache throughout. A first
jump of the day, against a `src.zip` no process has touched since boot, reads
those bytes off disk and is not measured here. 25.3 ms at p95 has 2× of
headroom against the budget and that is where the headroom would go, which is
the argument for moving the open off the request path if anything ever does.
The warm row has the same caveat one layer down: it reads a file this process
wrote seconds earlier, so a jump into a file a *previous* session extracted is
not measured either, and would be slower.

### The rest of the definition bench, against Phase 3a

The three existing rows are where `Definition`'s new `DocumentId` would show:
`javelin-semantic` clones one per answered definition, and `javelin-parser`
one per `definition_at`. p50 / p95, `Tier::Huge`:

| Row | Phase 3a | This run |
|---|---|---|
| `local` | 1.9172 / 2.2131 ms | 1.8613 / 2.1842 ms |
| `field` | 2.0554 / 2.3395 ms | 1.9719 / 2.3179 ms |
| `call` | 332.917 / 392.292 µs | 385.459 / 445.833 µs |

`local` and `field` came in slightly *under* Phase 3a at both percentiles, and
`call` 14–16% over it. `call` is the row that goes through `javelin-semantic`
and therefore the one where the clone lands, so the direction is at least the
right one to look at — but at 446 µs against a 50 ms budget it is 112× under,
and one run of one bench cannot separate 14% from a machine. Nothing is claimed
and nothing is ruled out, which is the same verdict the addressing slice
reached about the same `FileId` clone in a different bench.

### Completion, re-run

`ExternalMember` gained an `Option<ExternalSite>`, which
`member_completions_at` carries once per member of an external receiver, so
`cargo bench --bench completion` was re-run to see it. p50 / p95 at
`Tier::Huge`, against the external-members slice's run:

| Row | External members | This run |
|---|---|---|
| `completion warm` | 5.910 / 6.367 ms | 6.435 / 6.810 ms |
| `completion after_edit` | 399.236 / 410.205 ms | 411.766 / 439.387 ms |
| `member_warm` | 597.083 / 765.083 µs | 756.834 µs / 1.0799 ms |
| `member_god_warm` | 1.6815 / 1.9932 ms | 1.815 / 2.145 ms |
| `member_after_edit` | 401.897 / 410.473 ms | 414.747 / 447.969 ms |
| `member_external_warm` | 1.0403 / 1.2299 ms | 1.0651 / 1.2197 ms |

**The row the new field is actually in did not move.**
`member_external_warm` is +2.4% on p50 and −0.8% on p95, which is as close to
unchanged as one run gets — an `Option::None` beside two `String`s and a `Vec`
costs nothing worth measuring. Every other row is *above* the previous slice's:
3–27% on p50 and **7–41% on p95**, the wider spread being the one that matters
and `member_warm` the row at the top of both. That is the wrong direction for
an idler machine, which is why the table is here rather than a summary. It is also not attributable to this slice:
`member_warm`, the row that moved most, is completion on a file-local receiver
with two members and no `Externals` involvement at all, and nothing in this
slice is on that path. Same verdict the last two sections reached about the
same kind of spread — nothing claimed, nothing ruled out, and every row here
other than the two already-missed ones is 7× to 46× inside its budget.
`after_edit` and `member_after_edit` are those two; they are missed again, at
`Tier::Huge`, by the same whole-document `FileSyntax` and `ScopeTree` rebuild,
and no row changed status in either direction.

### Reading the JDK, re-measured

`reading_a_real_jdk_reports_its_cost` in `crates/classpath/src/sources.rs`,
`#[ignore]`d and run by hand, release build, three runs, same JDK 25 as both
previous rows:

| | Phase 3a | External members | This slice |
|---|---|---|---|
| Type facts produced | 24 467 | 24 467 | 24 467 |
| Member facts produced | none stored | 208 675 | 208 675 |
| Elapsed, single-threaded | 10.95 s (10.77, 10.95) | 11.41, 11.07, 11.03 s | **10.93, 10.88, 10.89 s** |

**Recording where a fact came from costs nothing this measurement can see.**
All three runs are below all three of the external-members slice's, and within
1% of Phase 3a's — but the previous slice's runs were taken on a machine with
two JDTLS instances and a Gradle daemon resident and these were taken on an
idle one, so the right reading is "no cost visible", not "the read got faster".
What is being added is one `String` field per type and two `u32`s per member,
off trees the pass had already built.

The test reports the provenance it produced, which is what sizes the cache
growth below: **1 331 857 bytes** of entry names, 24 467 of them averaging
54.4 characters, and **1 669 400 bytes** of member name offsets in memory,
two `u32`s for each of 208 675 members.

### The cache, and the warm read

Same method as the external-members slice: the release binary over real pipes,
driven by a script that sends `initialize`, then `initialized`, and reads the
server's own `classpath ready` log line — `duration_ms` and `source` — off
stderr. The cache file was deleted before the cold run, and schema 5 would have
discarded the version-4 file anyway.

| | External members | This slice |
|---|---|---|
| Cold, empty cache | 11 334 ms | **11 125 ms** |
| `jdk.sqlite` on disk afterwards | 27.7 MB | **30.7 MB** |
| Warm, same cache | 103–139 ms | **105–113 ms** |

The cold run's wall clock from `initialized` to the log line was 11 138 ms, and
the cache file is 30 732 288 bytes against the previous 27 709 440. The warm
figure is six consecutive runs: 113, 108, 105, 108, 108, 106.

**The cache grew 3.0 MB — 3 022 848 bytes — against the plan's Ruling 2, which
predicted roughly 4 MB and owed the real number.** The read reports 1.33 MB of
entry names, and the remaining 1.69 MB is the 417 350 offsets at about four
bytes apiece once SQLite's varints and its row overhead are counted — within a
per cent or two of the 1.67 MB the same integers occupy in memory. Against the
22 MB the member rows themselves cost in the previous slice it is a seventh as
much again, and it buys the whole feature: without it a jump would mean
re-parsing the entry on the request path and finding the member by name, which
costs a parse per jump and cannot tell two overloads apart.

**The warm read did not move**: 105–113 ms against 103–139 ms, a narrower
spread on an idler machine and the same number. It is still met against the
"warm startup in hundreds of milliseconds" target, and still not on any
request's path. The six runs were back-to-back, so the file was in the OS page
cache throughout.

### Still not measured, updated

- **What a session of jumping into the JDK leaves on disk.** Each jump into a
  new file writes that entry under `<cache root>/jdk-sources`, tens of KB at a
  time, and nothing prunes it. Bounded in practice by how many JDK files one
  person opens and unbounded in principle; no number here.
- **A jump into a file a previous session extracted.** The 39 µs warm row reads
  a file this process had just written, so the page cache held it.
- **The archive handle's own footprint.** A `ZipArchive` over 15 371 entries is
  resident for the rest of the session once the first jump opens it, and
  nothing profiles it — the same gap as the `TypeWorld` below, one layer down.
- **Local references.** Still no benchmark, because still no feature. It and
  the workspace source index are what remain of Phase 3b, and this slice
  brought neither.
- **The world's supertype closure walk.** Unchanged as a gap:
  `ClasspathExternals::members` closes over the graph and no benchmark
  exercises it on a deep real hierarchy.
- **Memory.** Unchanged as a gap and slightly larger as a question: the facts
  now carry an entry name per type as well.
- **Editor-observed latency.** Phase 3b's row in
  [`manual-testing-zed.md`](manual-testing-zed.md) is still owed and was
  deliberately not walked for this slice either: the phase's pass belongs at
  the end of the phase. What a walker should expect is a first jump into the
  JDK at around 24 ms on top of whatever the request itself costs, and every
  later jump into the same file at microseconds.

## Measured on Phase 3b's workspace-source-index slice

Measured on 2026-09-11, same machine as every table above, release build, idle.
One run, Criterion medians with its 95% confidence interval beside them; this
bench takes ten samples per row rather than the 200-sample loop the latency
benches use, because each sample indexes a thousand files and the question is a
ratio rather than a tail.

**Nothing in this slice is on the request path or the startup path, and no
budget's status changed.** The walk runs on a detached thread started from the
`initialized` notification, exactly as the classpath read does; no handler
reads what it produces yet. The four rows this document records as missed are
the same four.

### The new rows: indexing a workspace's own sources

`benchmarks/benches/workspace_index.rs`, group `workspace_index`, over a
generated tree of 1 000 files in ten packages — an ordinary domain class each,
with a package, two imports, a written supertype, a few members and a nested
interface, so 2 000 types in all. Both rows include the directory walk.

| | median | 95% CI | per file |
|---|---|---|---|
| `cold` — nothing stored, every file read and parsed | 82.5 ms | 80.6–83.7 ms | 82 µs |
| `warm` — every file's time and size already stored | 6.68 ms | 6.58–6.74 ms | 6.7 µs |

**12.4× is the whole point of the `source_files` table.** A warm session pays
one `stat` per file and opens nothing; a cold one reads, parses and writes
every one. Should those two numbers ever converge, the skip has silently
stopped working and every start is paying for a full parse of the project — the
reason the two rows are measured together rather than only the cold one.

Extrapolating linearly, a 10 000-file workspace costs about 0.8 s cold and
70 ms warm, and a 100 000-file monorepo about 8 s and 0.7 s. Nothing waits on
either: the editor is answering from the open buffer throughout, and the walk
is cancelled at shutdown within one batch of 256 files. The linear assumption
is the honest part to distrust here — one run, one tree shape, one filesystem.

### Still not measured, in this slice

- **What the index costs on disk.** The bench indexes into an in-memory
  database, because what it measures is the walk and the parse. A real
  workspace's `sources.sqlite` is unmeasured; the JDK's fact cache is 30.7 MB
  for 24 467 types with members and provenance, and this schema stores neither
  members nor text, so it should be far smaller per type — but that is an
  argument, not a measurement.
- **A walk over a real project.** The tree is generated, so every file is the
  same size and every directory holds a hundred of them. A real repository has
  a long tail of large files and a deeper tree.
- **The published index's footprint.** `WorkspaceIndex` holds every type's
  name, simple name, path and written supertypes in memory for the session,
  and nothing profiles it — the same gap as the `TypeWorld`'s.
- **Incremental maintenance.** There is none: the index is as old as the
  session once the walk finishes. A file saved during a session is not
  re-indexed, so there is no number for what that would cost.

## Measured on Phase 3b's workspace-symbol slice

Measured on 2026-09-11, same machine, release build, idle. Criterion medians
with its 95% confidence interval; the indexing rows above were re-run in the
same invocation and did not move (`cold` 84.7 ms, `warm` 6.74 ms — inside the
run-to-run spread of the numbers recorded above, which is why they are not
restated as a new baseline).

**No budget's status changed, and the four rows this document records as
missed are the same four.** What is new is the first request answered out of
the workspace index, and it is answered by a linear scan — so the row worth
watching is not how fast it is but what it is proportional to.

### The new rows: a symbol query over the index

`benchmarks/benches/workspace_index.rs`, group `workspace_symbol`, over the
same generated tree: 2 000 indexed types, `limit` 500, which is the cap
`handlers/workspace_symbol.rs` applies. Each row is one call to
`WorkspaceIndex::matching`, which is the whole of what the handler does before
it formats.

| | median | 95% CI | budget |
|---|---|---|---|
| `prefix` — `Type`, which every generated class starts with | 3.80 µs | 3.78–3.83 µs | 50 ms |
| `substring` — `isit`, inside `Visitor` and no prefix | 8.93 µs | 8.88–8.99 µs | 50 ms |
| `miss` — `zzzz`, which nothing matches | 7.46 µs | 7.40–7.54 µs | 50 ms |

**13 000× under the interactive budget at this size, and the size is the
point.** `prefix` is the fastest row because it stops at the cap after 500 of
2 000 names; `substring` and `miss` scan all of them, so those two are what
scale. At 3.7 ns per name a million-type monorepo is about 4 ms per keystroke —
still inside the budget, still linear, and the number that says when the scan
has to become a prefix index. The `ponytail:` on `matching` names that
upgrade, and this is the row that would justify it.

The formatting the handler adds on top is not measured separately: it is one
`Uri` built per row and at most 500 rows, against a scan over the whole
workspace. It would show up in the row above if it mattered.

### Still not measured, in this slice

- **The request end to end.** These rows measure the index query, not
  `workspace/symbol` over a pipe. The editor pass proves the answer arrives;
  nothing times it. The gap is the JSON serialisation of up to 500 symbols,
  which is the same shape as `documentSymbol`'s and is not on any missed row.
- **A query against an open buffer.** There is nothing to measure, because the
  handler does not consult one — the ceiling is
  `ponytail:`-marked on the handler, and the slice that closes it owes the
  number.

## Measured on Phase 3b's cross-file-name slice

**Nothing was measured, and that is the finding rather than an omission.** The
slice adds no row to any bench, and every row above is unchanged because
nothing it touches is proportional to anything.

Two pieces of work landed on the request path.

`Externals::resolve` now asks one table over the workspace's own types *and*
the JDK's instead of the JDK's alone, which is one extra `HashMap` lookup per
candidate fully-qualified name per JLS 7.5 tier — at most a handful per name,
each O(1) in the size of the workspace. The `completion` and
`overload_elimination` groups exercise that path and did not move; they are not
restated, because a number that has not changed is not a new baseline.

Definition and hover gained a fall-through that runs **only where they
previously answered null**: `javelin_parser::name_at` (a tree-sitter descendant
lookup, the same one `definition_at` already pays), `resolve_written`, and one
`WorkspaceIndex::with_fqn`, which is a `HashMap` get. The `definition` group
measures positions that *resolve*, so it never reaches the new code at all —
which is also why its numbers are unchanged, and why a row added here would
measure a hash lookup and call it a feature.

What would deserve a row is a reader whose cost grows with the workspace. The
one already here is `workspace_symbol`'s linear scan; `with_fqn` is not that,
and the slice that stores members — where a cross-file `.` has to close a
supertype chain across files — is where the next real number comes from.

## Measured on Phase 5a's reading-aids slice

Measured with `cargo bench -p javelin-benchmarks --bench semantic_tokens`,
release build, idle machine — Criterion's own regression run and the
benchmark's own 200-sample `Instant`-timed percentile loop, which land within
noise of each other at every tier. Budget: p95 < 50 ms.

`textDocument/foldingRange`, `textDocument/selectionRange` and
`textDocument/documentHighlight` have no benchmark of their own, for the same
reason `documentSymbol` does not: nothing calls them on every keystroke.
`textDocument/semanticTokens/full` does — the LSP client that requests it
re-requests it after every edit — which is why it is the one row this phase
added.

### The new row: `semanticTokens/full`

| Tier | p50 | p95 | p99 | max | Budget (50 ms) |
|---|---|---|---|---|---|
| `small` | 138.25 µs | 149 µs | 157.9 µs | 401.1 µs | met, 336× under |
| `medium` | 3.171 ms | 3.299 ms | 3.467 ms | 6.439 ms | met, 15× under |
| `large` | 76.99 ms | 78.29 ms | 79.79 ms | 110.8 ms | **missed, 1.57×** |
| `huge` | 4.853 s | 5.472 s | 7.837 s | 12.16 s | **missed, 109×** |

Criterion's own means agree in shape: 141.16 µs, 3.198 ms, 76.15 ms and 5.034 s
across the same four tiers.

**Those two misses are unlike every other missed budget in this document, and
the difference is where the time is spent.** Completion's 417.9 ms at
`Tier::Huge` is a reparse and a view rebuild on the parse pool: the request
loop is free throughout, and the cost is a stale answer rather than a stopped
server. This one is on the loop. `crates/server/src/lib.rs` is single-threaded
and `$/cancelRequest` arrives on the very `connection.receiver` the loop is
blocked on while a handler runs, so the per-node `token.check()` inside
`javelin_parser::semantic_tokens` — correct discipline for a future that
dispatches requests off the loop — **cannot fire for the request currently
executing**. 5.472 s at `Tier::Huge` is therefore not a slow answer: it is
hover, definition, completion, `didChange` and every finished reparse waiting
that long, and an editor re-issues `semanticTokens/full` after every edit.

### The guard: `MAX_DOCUMENT_BYTES`

*Superseded by the references slice below, which made the walk linear and
moved the guard to 640 KiB; this is the account of why it was put there.*

`crates/server/src/handlers/semantic_tokens.rs` refuses a document larger than
**128 KiB (131 072 bytes)**, answering an empty token list without walking and
logging the size and the threshold at `debug`. The number is read off the
table above rather than picked: the generator's tiers measure

| Tier | bytes | p95 | side of the guard |
|---|---|---|---|
| `small` | 2 811 | 149 µs | walked |
| `medium` | 53 001 | 3.299 ms | walked |
| `large` | 539 601 | 78.29 ms | refused |
| `huge` | 5 513 601 | 5.472 s | refused |

so 128 KiB sits 2.5× above the largest tier that must keep working and 4×
below the smallest that must not run. Interpolating the two measured points
across that gap (the cost grows as roughly bytes^1.37 between them) puts the
worst case at the threshold itself near 11 ms — inside the 50 ms budget with
room for a machine slower than this one. `benchmarks/benches/semantic_tokens.rs`
asserts which side of the guard each tier falls on, so a fixture that grew or
shrank across it fails the bench rather than silently changing what the rows
measure; `large` and `huge` now time the guard refusing rather than the walk
running, and the numbers above are what justified it.

What a file above the threshold loses is semantic colouring, and what it falls
back to is the client's own TextMate grammar — a plainer rendering, not a
wrong one, which is the fallback Ruling 5 already blesses for anything this
engine cannot justify. The threshold is a stopgap for the real fix below, not
a substitute for it.

### Why it costs that, and what the real fix is

**The cause is not the one the plan that
scheduled this phase predicted.** That plan expected an unbounded walk —
"more work per request as the file grows without anything capping it" — and
named `semanticTokens/full/delta` as the fix. Neither holds up under the
measured numbers. Visiting every node in the tree is linear in file size, and
delta would not help even if the walk were the problem: LSP's delta variant
still computes the full current token list before diffing it against the
previous `resultId`, so whatever this handler costs today it costs again on
every delta request too.

The real cause is `ScopeTree::find` (`crates/parser/src/scopes.rs`): a
reversed linear scan of one scope's `bindings`, comparing names one at a time.
`benchmarks/src/lib.rs`'s generator declares one field per generated method,
all directly in the class body — one flat scope, 20 000 bindings at
`Tier::Huge` against 200 at `Tier::Medium`. This handler resolves every
field's own declaration and its one use through that scope, so the total cost
is quadratic in the field count rather than linear in the file. The measured
ratios say so: a 100× method-count increase from `Tier::Medium` to
`Tier::Huge` produced a ~1660× time increase, and a 10× increase from
`Tier::Large` to `Tier::Huge` produced ~70× — both far past what a linear walk
would produce from the same size change. The upgrade, named on the handler's
own `ponytail:` in `crates/server/src/handlers/semantic_tokens.rs`, is inside
`ScopeTree` itself: an index per scope (a name-to-binding map), or memoising a
resolution repeated within one walk. Rewriting `ScopeTree` construction is
outside this phase and deserves its own slice with its own benchmark — it
would speed up definition, hover, completion and documentHighlight in the same
stroke — so this phase ships the guard, which is the smaller change that stops
a generated god-class from freezing the loop, and leaves the quadratic scan
where it is. Raising or removing `MAX_DOCUMENT_BYTES` is what that slice buys.

## Measured on Phase 3b's references slice

Measured on 2026-09-24, same machine as every table above, release build.
Latency rows are the benches' own `Instant`-timed percentile loops beside
Criterion's runs; the machine was not idle for every run, which the p95/p50
spread on some rows shows, and the rows below say where that matters.

**Two budgets changed status, both for the better, and one new request met
its budget at every tier but the one generated god-class every budget in this
document misses at.**

### `ScopeTree` and `FileSyntax::declaration_at`, indexed

The references walk resolves every identifier of every workspace file, and two
linear searches made that quadratic in a flat class: `ScopeTree::find` scanned
a scope's bindings one at a time, and `FileSyntax::declaration_at` — which
`definition_at` calls once per token — searched every declaration in the file.
`ScopeTree` now keeps each name's bindings as a sorted `(scope, binding)` run
and binary-searches a scope's children; `declaration_at` descends by span,
licensed by an ordering check made once per build. Both are pinned by
equivalence tests against the old code at every offset of their fixtures.

The semantic-token walk beneath the handler's guard, 20 samples per tier
(`semantic_tokens walk`):

| Tier | bytes | before, p95 | after, p95 | Budget (50 ms) |
|---|---|---|---|---|
| `small` | 2 811 | 139 µs | 145 µs | met |
| `medium` | 53 001 | 3.28 ms | 2.97 ms | met |
| `large` | 539 601 | 77.5 ms | 30.5 ms | **met — was missed** |
| `huge` | 5 513 601 | 5.39 s | 357 ms | missed, 7× |

`Tier::Medium` to `Tier::Huge` is 100× the methods and now ~120× the time,
where it was ~1660×: linear. `MAX_DOCUMENT_BYTES` moved from 128 KiB to
**640 KiB**, read off the `large` row — linear from 30.5 ms at 539 601 bytes
puts 640 KiB near 37 ms — so `Tier::Large` is coloured and `Tier::Huge` still
answers an empty list. What is left at `Tier::Huge` is a walk proportional to
a 5.5 MB file on the request loop, and the upgrade named on the handler is to
run it off the loop or to answer `range` only.

### The new rows: `textDocument/references`

`benchmarks/benches/references.rs`, group `references`, through the handler
on the generated fixture, cursor on the middle method. Budget: the 100 ms
local-references row, which had never had a number.

| Tier | `local` p95 | `field` p95 | `call` p95 | Budget (100 ms) |
|---|---|---|---|---|
| `small` | 13.0 µs | 7.1 µs | 7.5 µs | met |
| `medium` | 14.5 µs | 15.8 µs | 19.5 µs | met |
| `large` | 17.7 µs | 125.5 µs | 98.0 µs | met |
| `huge` | 44.5 µs | 561.7 µs | 1.92 ms | met, 52× under |

Every row finds exactly what the fixture writes — four for `local`, two for
`field` and `call` — which the bench asserts before it times anything.

**The first version of this handler missed at `Tier::Large` and did not
finish at `Tier::Huge`,** and the three fixes are the reason the table reads as
it does. Measured along the way, p95: `local` at `Tier::Large` was 1.28 s with
`declaration_at` still a search, 34.9 ms once it descended, and 256 µs once a
local's search was bounded by its scope; `call` at `Tier::Huge` was 527 ms with
candidates found by a walk that collected every node's children, 253 ms with
that walk allocation-free, and 1.92 ms with candidates found by a substring
search over the text and confirmed by one descent into the tree per hit. The
last is the one that generalises: finding a spelling needs the text, not the
tree.

`local` is the in-file half at its worst case for spelling — every generated
method declares its own `local`, so the file holds four per method — and it is
the one the scope bound answers: a local can only be named inside the scope
that binds it, so only that stretch of the file is searched. `field` and
`call` have one use each and are searched through the whole file, because a
field or a method can be selected from anywhere in it; what they cost is the
walk over every token plus one definition resolution per same-spelled token.

`references_workspace`, the cross-file half, is the loop the handler runs over
the index — every occurrence spelled like the target, resolved through
`javelin_classpath::simple_name` against its own file's package and imports —
over a generated index of 2 000 files naming the target ten times each, so
20 000 rows that all resolve:

| rows | p50 | p95 | max | Budget (100 ms) |
|---|---|---|---|---|
| 20 000 | 2.83 ms | 4.25 ms | 5.99 ms | met, 24× under |

Linear in the rows spelled like the target, not in the workspace: a type
nobody names costs nothing, and a million-row monorepo type is the case that
would want the rows narrowed by file before they are resolved.

### Indexing, re-measured

The walk now builds a `ScopeTree` per file and writes one row per type-like
name, and the warm read loads those rows too. `workspace_index`, measured back
to back against the commit before the slice on a machine that was not idle —
both absolute numbers are about twice what the workspace-index slice recorded,
so the ratio is the finding:

| | before | after | ratio |
|---|---|---|---|
| `cold` | 184.3 ms | 273.1 ms | 1.48× |
| `warm` | 8.78 ms | 20.3 ms | 2.31× |

Applied to the quiet-machine figures that slice recorded, that is roughly
120 ms cold and 15 ms warm for a thousand files. Nothing waits on either: both
run on the detached thread started from `initialized`.

## Measured on Phase 3b's workspace-members slice

Measured on 2026-09-24, same machine, release build, on a quieter machine than
the references slice's runs.

**No budget's status changed.** One new row is on the request path and meets
its budget by 71×.

### The new row: a member closure over a workspace receiver

`benchmarks/benches/workspace_index.rs`, group `workspace_members`: what a `.`
on a workspace receiver asks `ClasspathExternals::members` for — a type of
2 000 fields under a chain of 20 workspace supertypes of ten methods each, so
2 200 members, every member's written types and every frame's supertypes
resolved against their own file as the walk visits them. No JDK world behind
it, so the implicit `Object` at the top is a gap rather than a frame.

| members | p50 | p95 | max | Budget (50 ms) |
|---|---|---|---|---|
| 2 200 | 684 µs | 703 µs | 764 µs | met, 71× under |

Proportional to the members visited, like the JDK closure it sits beside; the
per-member cost is one `resolve_member_type` per written type.

### Symbol search over members

`workspace_symbol/members_prefix` and `members_miss`, over the same
thousand-file tree's members — every field and method, private ones included:
**8.6 µs** for a prefix and **32 µs** for a query nothing matches, the full
scan. Linear in the members, which outnumber the types several times over;
extrapolated, a million members is a few milliseconds per keystroke, still
inside the budget, and the prefix index `matching`'s `ponytail:` names is the
upgrade for both.

### Indexing, re-measured

`workspace_index` over the same thousand-file tree, which now also records
each type's members:

| | median |
|---|---|
| `cold` | 134.4 ms |
| `warm` | 12.6 ms |

Against 82.5 ms and 6.68 ms when the index recorded types alone, three slices
ago: the references slice's occurrence table and this slice's member table
together cost about 1.6× cold and 1.9× warm. Both off the request path, on the
detached thread started from `initialized`.

## Measured on Phase 3b's member-references slice

Measured on 2026-09-28, same machine, release build.

**No budget's status changed.** The new row is off the request path, and the
request-path rows the slice touched still meet their budgets.

### The new row: the member-resolution pass

`benchmarks/benches/workspace_index.rs`, group `member_resolution`: every
selected member of every file of a generated thousand-file tree resolved with
definition's own `external_member_at`, which is what the workspace worker runs
after the walk, and again for each changed file and its dependents. Each file
holds a peer of the next file's type, in the next package, and selects four of
its members — `peer.size()`, `peer.peer`, the `size()` after it and
`peer.chain()` — so all 4 000 selections cross a file, which the bench asserts.
The text is read before timing, and there is no JDK world behind it.

| files | selections | median |
|---|---|---|
| 1 000 | 4 000 | 70.5 ms |

About 18 µs a selection, parsing and view-building included. It read 60.0 ms
before the review pass made every selection try definition's local chain
first — which is what records the declaring file's own calls — and the
difference is that chain answering "not this file's" 4 000 times. It has no budget
— nothing waits on it, and until it lands references list the open file's uses
alone — but it is what a type every file depends on costs per edit to it, since
every dependent is re-resolved in that batch.

### What else moved

`references_workspace/type` over 20 000 occurrence rows: **2.44 ms**, against
4.25 ms when it was written, and the per-tier `references` rows are unchanged
in shape — p95 1.07 ms for a method at `Tier::Huge`. The member half of a
references query is one `member_refs_to` lookup plus, for a target declared
elsewhere, a re-resolution of the open file's same-spelled selections, which
is bounded by the spelling rather than by the file.

`workspace_index` cold and warm did not move: the slice adds two `DELETE`s per
changed file and a read of the new table on publish, and back to back against
the commit before it the tree indexes in 145.6 ms against 143.2 ms cold and
15.4 ms against 15.0 ms warm. A first run of the day read 189.6 ms cold; the
back-to-back pair is the comparison, and it is noise.

## Measured on Phase 4's dependency-classpath slice

Measured on 2026-09-28, same machine, release build.

**No budget's status changed.** Nothing new is on the request path: the build
tool and the JAR reads run on the classpath thread after the JDK's world is
published.

### The new rows: reading a JAR

`benchmarks/benches/dependencies.rs`, group `dependency_jar`: a JAR of 2 000
class files — the committed `Box.class` fixture renamed into 2 000 packages —
read into facts, and the same facts read back out of the per-JAR store.

| | median |
|---|---|
| `read` | 29.9 ms |
| `cached` | 8.1 ms |

3.7×, which is a smaller margin than the JDK's store buys — a class file is a
declaration format, so there is no parse to skip, only a decompress and a
walk of the constant pool. A real JAR, by the `#[ignore]`d
`reading_a_real_jar_reports_its_cost`: Guava 28.2 is 1 349 types and 10 770
members in **59 ms**. So a project of two hundred JARs pays low single-digit
seconds on its first session and well under that after, off the request path
either way.

What is not measured: the build tool itself, which is Maven's or Gradle's cost
and seconds to minutes; and the store's file open per JAR, which the
`cached` row excludes.

## Measured on Phase 3d's inverse-edges slice

Measured on 2026-09-28, same machine, release build.

**No budget's status changed.** Two new rows, both on the request path.

`benchmarks/benches/workspace_index.rs`, group `hierarchy`: an interface
that a thousand workspace types in ten packages implement through an import.

| | median | Budget (50 ms) |
|---|---|---|
| `implementations_of_a_type` — every subtype, transitively | 465 µs | met, 108× under |
| `overrides_of_a_method` — every override of its one method | 1.05 ms | met, 48× under |

Linear in the candidates: each is found by the simple name it writes and
kept after its written supertypes are resolved against its own file, and an
override additionally costs each subtype's own members — its own frame, not
its whole closure, which the review pass changed and which took the row from
1.53 ms. The subtype row went the other way, from 271 µs, for stripping type
arguments per segment rather than at the first `<`. The call hierarchy has no
row: an expansion is one `member_refs_to` lookup plus a binary search of each
calling file's members, and outgoing calls a range of the file's references
sorted by offset — bounded by the answer.

## Measured on Phase 3c's unresolved-types slice

**No row, and no budget's status changed** — recorded rather than owed, as
the cross-file-name slice's was. The judgement is file-sized work, a parse
and a resolution per type name, and it runs on a diagnostics thread of its
own, debounced by `diagnostics.debounceMs` (250 ms), so nothing interactive
waits on it: a slow judgement is a late squiggle, never a stopped loop. What
the slice puts on the request loop is bounded by the answer — an `Arc` clone
of the world per `didOpen` and `didChange`, one per open document when the
classpath or the workspace index arrives, and a version compare per
published result. The row that would be owed is the judgement at
`Tier::Huge`, the day a user can see the delay; the function is
`pub(crate)` in `javelin-server`, so it would come with an export for the
bench.

## Rules

- Do not optimise on intuition when a benchmark can answer the question.
- A change that claims to be faster comes with the measurement.
- Adding a dependency means accounting for its cost at startup and at runtime.
- A slow p99 is a bug even when p50 is fine.

## Measured on Phase 3c's unresolved-methods slice

**No row, and no budget's status changed**, for the first slice's reason: the
member pass runs in the same judgement on the diagnostics thread — every
selected name the file makes, each receiver's closure asked once per file
through the member-reference pass's cache — so a slow file is a late
squiggle and never a stopped loop. What the slice adds to the request loop is
bounded by the world rather than by a request, and paid once per arrival
rather than per keystroke: a scan of every workspace file's imports for a
member generator when the index is published, and a prefix scan of the fact
set when the classpath is. The JDK's cache is read again once, for schema 14:
the real JDK 25 read measured 15.6 s here against the recorded 10.9–11.4 s.
The slice adds one `has_errors` per entry, which reads a flag on the tree's
root, so the gap is not attributed to it — but it was not re-measured
against the previous commit either, and is recorded as a single run.

## Startup at the first release, 2026-10-01

Release build (`cargo build --release -p javelin`) on the development
machine, after slices 1–3 of the first release. `scripts/bench-startup.sh`
fell back to its plain loop, hyperfine not being installed: **mean 13 ms over
100 runs**. The same binary timed per run from a script after warm-ups — 200 runs of
`--version`, 100 of `serve`:

| Measurement | Median | p95 | Max | Budget |
|---|---|---|---|---|
| `javelin --version`, spawn to exit | 8.2 ms | 9.8 ms | 12.8 ms | < 50 ms ✅ |
| `javelin serve`, spawn to the `initialize` response's first byte, 100 runs | 8.2 ms | 11.3 ms | 12.8 ms | < 100 ms ✅ |

Nothing on that path opens the cache, the JDK or the workspace, which is why
the generic facts and the lambda and override work of the three slices did
not move it.

## Measured on Tier 1's inlay-hint slice, 2026-10-05

`benchmarks/benches/inlay_hints.rs`, `--quick`, warm, the fixture's calls
given a literal argument so each one is resolved and hinted:

| Tier | Screen (60 lines) | Whole file |
|---|---|---|
| Small | 37.7 µs | 85.6 µs |
| Medium | 109 µs | 3.86 ms |
| Large | 802 µs | 20.1 ms |
| Huge | 19.7 ms | 20.9 ms |

Every row meets the 50 ms budget. The whole-file rows from `Tier::Large` up
are the handler's own bound, not the work a whole file would take: the call
walk stops at 500 calls and resolution at a 20 ms deadline, so a request for
a generated file answers its first calls and the next scroll answers the
rest. A call costs about 4 ms at `Tier::Huge` — the receiver's member walk
over 20 000 members, twice, plus `sole_candidate` — which is what puts a
Huge screen near the deadline. The first request after opening a file pays
for its view as any request does: 38 ms at `Tier::Large` and 396 ms at
`Tier::Huge`, the first-parse ceiling recorded above.

## Measured on Tier 3's type-hint slice, 2026-10-09

`benchmarks/benches/inlay_hints.rs`, warm, mean. The fixture's `var` now
reads its `int` local, so every generated method carries a type hint beside
its call's parameter hint, and the bench asserts both kinds appear:

| Tier | Screen (60 lines) | Whole file |
|---|---|---|
| Small | 69.9 µs | 145 µs |
| Medium | 154 µs | 5.01 ms |
| Large | 971 µs | 20.1 ms |
| Huge | 21.2 ms | 21.4 ms |

Every row meets the 50 ms budget. The first version missed it on a whole
`Tier::Huge` file: the chain walk found no chain and so visited every node
of a 5.5 MB tree before the first job ran, which left the 20 ms deadline
spent and the answer empty. Every walk now stops where the call walk's
500-call cap stopped it.

## Measured on Tier 3's ranged-tokens slice, 2026-10-09

`benchmarks/benches/semantic_tokens.rs`, the new `range` rows: 60 lines
ending one line before the end of the file, p95, against `full` on the same
tier. The range is at the bottom on purpose: skipping a node is a scan of
its siblings, so the last screen of a flat class body is the worst place.

| Tier | `range`, 60 lines | `full` |
|---|---|---|
| Small | 66.5 µs | 146 µs |
| Medium | 95.5 µs | 3.05 ms |
| Large | 399 µs | 32.1 ms |
| Huge | 2.46 ms | guarded to empty; the unguarded walk is 349 ms |

Every `range` row meets the 50 ms budget, `Tier::Huge`'s by 20×. That is
the file the 640 KiB guard refuses `full` for, and it is now coloured a
screen at a time. The guard applies to the range's own size, so a client
asking for a range over 640 KiB gets the empty list `full` gets.

## Measured on the cold-session references fix, 2026-10-12

A reviewer opening a fresh git worktree waits for cross-file references
from a cold session: the workspace index and the Gradle classpath are
keyed by the root's path, so every worktree pays the walk, the build tool
and the member-reference pass. Measured on the sample project (a private Spring Boot codebase), 6 701 files,
12 cores, release build, the JDK's facts cached:

| Stage | Before | After |
|---|---|---|
| Workspace walk, cold | 7.3 s | 2.1 s |
| Member-reference pass, cold | 19.9 s | 7.1 s |

The walk parses on every core but one and stores on one thread, in
`CacheDb::walk_into_store`. The pass was allocation-bound: every lookup
of a member cloned the type's whole external member list, so
`ExternalMembers::members` is a `Vec<Arc<ExternalMember>>` now and a
clone is a pointer bump each (19.9 s to 7.9 s). The rest is
`is_processor_output`, which every lookup asked of every path and which
now rejects a path without `generate` in it before splitting it (7.9 s to
7.1 s). In an editor session the critical path is now Gradle's own ~11 s
plus the pass, about 18 s from opening to cross-file references, against
about 33 s. Nothing on the request path moved.

The wait is shown too: the walk, the build tool, the sources graft and
the pass are LSP work-done progress, which Zed shows beside the server's
name, so a reviewer can see when references will cross a file.

## Measured on resumable inlay hints, 2026-10-13

The deadline is unchanged at 20 ms. What changed is what a request
stopped by it leaves behind. Its jobs' answers are kept per document,
and a client that takes `workspace/inlayHint/refresh` is sent one. Its
next request skips what is kept and carries on.

Before the change, every whole-file request of one of the
sample's service files (462 lines) stopped around line 206:
- 22 or 23 type hints, depending on whether the member-references pass
  overlapped the request;
- every hint below that line was never shown.

After it, three runs warm gave the same result each time:
- three asks, 46 type hints;
- the slowest `inlayHint` request 20.5 and 20.6 ms in two of the runs,
  the deadline plus one job;
- the other three files answered in one ask each.

No benchmark row moved, because a request does the same work under the
same deadline. A memo hit is two hash lookups.

## Measured on Tier 4's slice 7, `@SuperBuilder` and `@Singular`, 2026-10-12

The slice adds nothing to the request path. Its new rules run in the walk,
once per file that writes the annotations. Its new types are read through
the member closure as any workspace generic is. No row was added.

On the sample the walk stores 8 536 types, against 8 508: these are
the builders of its `@SuperBuilder` classes. The schema bump re-reads every
file. That first walk took 2.42 s, and the warm one after it 0.98 s. The
worker's first full pass over 6 701 files took 5.2 s, against 5.4 s for
slice 5.

The bump also discards the JAR stores, so the first session re-grafts every
sources JAR. Until that lands, it answers 1 126 of the smoke's 1 604 tokens
rather than 1 141; the 15 it misses are JAR type names. Warm, it answers
1 141 again.

A probe over the 12 files that start a chain at a `@SuperBuilder` class's
`builder()` asked definition at each link. All 312 answer, against none
before the slice. Each of the 258 setters lands on a field of its name.

## Measured on Tier 4's slice 6, explicit constructor calls, 2026-10-12

Definition and hover at a `this` or `super` keyword select among one type's
constructors, as a `new` already did. References on a constructor make one
new walk: they scan the open file for explicit calls and constants, beside
the `new`s they already scanned, and read every other file from the index.

The scan is the new `references_explicit_calls` row, against the 100 ms
references budget:

| Tier | p95 |
|---|---|
| Small | 46.9 µs |
| Medium | 772 µs |
| Large | 7.71 ms |
| Huge | 78.7 ms |

Its first version asked every node for its parent. Tree-sitter finds a
parent by searching down from the root, so that walk was quadratic in a flat
class body. The slice's review found it and it was not measured. The scan
now reads each call from the call node down, and the table is linear in the
file. Each span it finds is sifted by its type's name before it is selected,
so an enum's constants are not each selected against another type's
constructor.

A probe over the sample's 270 files writing `super(` or `this(` asked
definition at each keyword, warm and cold: 175 of 330 answer, against none
before the slice.

## Measured on Tier 4's slice 5, references on library methods, 2026-10-12

The worker now records a call into a JAR member, and into a JDK member of a
type the workspace extends. On the sample the table holds 297 044
rows against 204 284, of which 92 453 are keyed to 2 171 library members.
Every JDK call would have added 52 649 more, which the closure rule keeps
to 501. The first full pass over 6 701 files costs 5.4 s, against 5.6 s
before the slice: the calls were resolved before, and only their rows are
new.

The widest answer is on the request loop. References on Mockito's
`when(T)` list 13 559 calls in 874 files, measured over real pipes from the
request to the parsed response: 34.5–39.6 ms over seven asks, against the
100 ms references budget.

## Measured on Tier 4's slice 4, bare inherited fields, 2026-10-12

The worker now resolves every expression name the scope tree leaves
unbound, and each one asks what the types around it hold as fields. The
first version walked those types' members per name, so a god-class that
writes a supertype was quadratic. `member_resolution/bare_names_in_a_subclass`
(`benchmarks/benches/workspace_index.rs`) is one 4 000-method class
extending a workspace type, naming two of its fields bare and two type
names in every method:

| Build | Time |
|---|---|
| As first committed | 3.69 s |
| Memoised per `Files`, members bisected by offset | 422 ms |

What each type holds as fields, and which field a name picks there, is
read once per query (`infer::FieldFrame`), and `enclosing_types` bisects a
type's members for the offset where the file's declarations are in order.
References share one `Files` across their tokens, so they pay the walk
once per request. On the sample the first full pass over 6 701 files
costs 5.6 s, against 7.2 s as first committed and 6.2 s before the slice.
