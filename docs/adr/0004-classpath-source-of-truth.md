# 0004 — The classpath's source of truth: the JDK's `src.zip`

**Status:** accepted, 2026-09-07

## Context

Phase 2 resolved names against the open buffer and nothing else. That ceiling
cost more than navigation into the JDK: it cost *elimination*. Overload
resolution has to be able to say "an `int` is not a `String`", and a name the
buffer cannot resolve could mean `Object`, `Number`, `Comparable` or
`Serializable`, each of which takes an `int`. So `f(int)` against `f(String)`
at `f(1)` answered nothing, and a domain type declared
`class Foo implements Serializable` was separable from nothing at all, because
its supertype closure left the file into the unknown.

Closing those needs facts about names the buffer does not declare, and the
request path only ever asks two questions of them: does this fully-qualified
name exist, and can one type be a subtype of another. Resolving what a *simple*
name means is a rule layered over the first question, not a third one. No
member signatures, no bodies, no bytecode semantics. The question is which
format those facts are read out of.

Options considered: the JDK's `lib/src.zip`, a `.class`/`jimage` reader,
`lib/ct.sym`, shelling out to `javap`, and a hardcoded `java.lang` table.

## Decision

Read `$JAVA_HOME/lib/src.zip` with the Tree-sitter parser this repository
already has. `javelin_classpath::read_type_facts` walks the archive, parses
each entry with `JavaParser`, projects it with `FileSyntax`, and emits one
`TypeFacts` per declared type: its fully-qualified name, its simple name, its
kind, its type-parameter count, and its direct supertypes resolved to
fully-qualified names. `javelin-index` caches the result in SQLite keyed by
JDK identity; `TypeWorld` is the immutable in-memory index the request path
asks its two questions against.

## Why

**No second Java front end.** The reader is `JavaParser` plus `FileSyntax`,
both unchanged. Every other option means a second thing in this repository
that claims to understand Java, and two readers that can disagree about the
same language is a class of bug with no natural home.

**One resolution rule, not two.** The JDK's own `extends` and `implements` are
names written in a compilation unit with a package and imports, exactly like
the user's. `javelin_classpath::simple_name` resolves both, so the rule that
enforces *ambiguity answers nothing, never a guess* is one function with one
set of tests rather than a source-form rule and a binary-form rule that drift.

**The facts arrive in source form.** `java.util.Map.Entry`, not
`java/util/Map$Entry`. That is the form a buffer's names are written in and
the form the cache stores, so no layer has to translate between two spellings
of the same type.

**One small dependency.** `zip`, for the archive. Nothing else.

## Alternatives rejected

**A `.class` or `jimage` reader.** It is the more complete answer — it works
on a JDK with no sources and it is the shape Phase 4 needs for dependency
JARs — and it is a new binary format, a constant-pool and signature-attribute
parser, and a second Java front end to keep honest, for facts src.zip already
carries. If Phase 4 builds one for JARs it can sit *beside* this reader rather
than replace it, which is when this record gets revisited rather than
reversed.

**`lib/ct.sym`.** The signature data `javac --release` compiles against:
compact, and it does not depend on the JDK shipping its sources. It is also an
internal, undocumented container of class-file-shaped signature data, keyed by
release rather than by the JDK in front of us — so it is the `.class` reader's
cost plus a layout with no compatibility promise.

**Shelling out to `javap`.** It needs a JDK that can *run* on this machine
rather than one that can merely be read; its output is formatted for humans
and carries no compatibility promise, which is a poor contract to put 24 467
types through; and it makes the reader untestable without an installed JDK.
The same argument already kept `/usr/libexec/java_home` and `java -version`
out of `javelin_classpath::discover`.

**A hardcoded `java.lang` table.** Rejected in Phase 2c and rejected again
here. A table has no way to stay honest against a same-package sibling type,
an on-demand import or a file-local `class String`, so it makes the ambiguity
invariant depend on a guess about what a name means. It also cannot answer the
supertype question at all, which is what `class Foo implements Serializable`
needed. One fully-qualified name *is* written down — `java.lang.Object` in
`crates/semantic/src/apply.rs` — because JLS 4.10.2 is true without reading
anything and nobody writes `extends Object` for the walk to follow; even that
name is *asked* of the world rather than trusted, so a world that does not
hold it excludes nothing.

## Costs

**Eleven seconds, once per JDK.** Reading Temurin 25's `src.zip` (15 371 zip
entries, 53.6 MB) yields 24 467 type facts in 10.95 s single-threaded, at
~76 MB peak RSS. Almost all of it is Tree-sitter parsing 15 000 files one at a
time; the zip I/O and the resolution pass are not the cost. It is background
work on a detached thread, cached across restarts, and the warm path is
15–38 ms.

**Startup cost: none, measured.** Nothing on the path to the `initialize`
response opens the zip, spawns a process or walks a filesystem looking for a
JDK. Discovery and the read start from the `initialized` *notification*.
Process startup measures 10 ms against 11 ms for a binary built from the
parent commit, despite the binary growing 3.37 MB → 5.62 MB when the server
picked up `javelin-index` and its bundled SQLite.

**No `src.zip` means no classpath.** A JRE, a `jlink`ed runtime image or a
distribution that ships sources separately yields no facts at all, and every
feature degrades to exactly Phase 2's answer: resolution from the open buffer
alone, and an honest refusal where two candidates cannot be separated. That is
a narrower server, not a broken one, and it is the same code path a machine
with no JDK takes.

**The facts are as good as the sources parse.** A supertype name that resolves
ambiguously or not at all is dropped and `TypeFacts::supertypes_complete` goes
false, which `TypeWorld::is_subtype` reads as *cannot say* rather than *no* —
so an incomplete closure costs a missing exclusion, never a wrong one.

## Revisit if

Phase 4 builds a `.class`/JAR reader for dependencies. Then src.zip is one
source of facts among several, and the interesting question becomes which one
wins for a type both describe — a new record, not an edit to this one.

Or if the 10.95 s read becomes visible to a user. The first lever is
parallelising the per-entry parse, which is embarrassingly parallel and needs
no new format.

## Amendment, 2026-09-09 — Phase 3b reads members too

The decision stands unchanged: `src.zip` is still the source, still read with
`JavaParser` and `FileSyntax`, still cached by JDK identity. Two statements
above were true when this was accepted and are not now, and they are amended
here rather than rewritten in place, because an ADR records what was decided
when.

**"The request path only ever asks two questions… No member signatures."**
It asks three. Phase 3b's external-members slice added *what members does this
type have*, and `read_type_facts` now emits a `MemberFacts` per member a type
declares — name, kind, staticness, declared type, parameter types in order,
variadicity — beside the `TypeFacts` described under "Decision". Parameter
*names* are still dropped, and there are still no bodies and no bytecode
semantics, so the "no second Java front end" reasoning is untouched: the extra
facts come off trees the reader already built, and their type names go through
the same resolution pass the supertype names already went through.

What it cost, measured: the read went from 10.95 s to 11.0–11.4 s — a real 1–4%
and not a run-to-run wobble, since every new run is slower than every 3a run —
and the SQLite cache from 5.7 MB to 27.7 MB
for 208 675 member rows beside the same 24 467 type rows. The warm read startup
pays went from 15–38 ms to 103–139 ms. Numbers and method:
[../performance.md](../performance.md).

**Member types are resolved at read time**, which is the same ruling as for
supertype names and taken for the same reason. Storing a member's type as the
author wrote it would need every declaring file's package and import list in
the cache as well, and would put name resolution on the request loop once per
member of every receiver.

"Revisit if" is unchanged, and the 10.95 s in it should now be read as
"about eleven seconds".
