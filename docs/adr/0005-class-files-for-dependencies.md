# 0005 — Dependencies are read from class files, and the build tool says which

**Status:** accepted, 2026-09-28

## Context

Phase 4 makes a name a JAR declares resolve. Two questions come with it: which
JARs, and what to read out of each one.

[ADR 0004](0004-classpath-source-of-truth.md) chose the JDK's `src.zip` over a
class-file reader, on the grounds that the repository should not grow a second
Java front end. A dependency usually has no sources beside it — a
`-sources.jar` is published for many artifacts and downloaded for few — so the
same choice is not available for a JAR, and "Javelin reads class files" reads
like a reversal of 0004. It is not one, and this record says why.

## Decision

**Which JARs: ask the build tool.** Maven's `dependency:build-classpath`, or a
Gradle init script that prints each project's `compileClasspath` and
`testCompileClasspath`, run once on a background thread and cached keyed by
the build files' modification times and sizes. Javelin does not model
dependency mediation, BOMs, profiles or version catalogues.

**What to read: the class file's declarations.** The constant pool, access
flags, `this_class`, `super_class`, `interfaces`, each field's and method's
name, descriptor and flags, and the `Signature` and `InnerClasses` attributes.
The output is the `TypeFacts` and `MemberFacts` the `src.zip` reader already
produces, stored in the same kind of store.

## Why this is not a second front end

A front end parses a language and resolves its names. A class file has
neither step left in it: every name is already fully qualified, every
supertype already resolved, every type already erased where it will be
erased. Reading one is closer to reading the fact cache than to parsing Java,
and nothing in it can disagree with `JavaParser` about the language, because
none of it is Java source.

`src.zip` stays the JDK's source of truth, for 0004's reasons and one more:
the sources carry parameter names and javadoc, and the class files do not.

**The one place a class file can mislead is erasure,** and the reader treats
it as the soundness rule it is. A method's descriptor is erased — `List.get`
returns `Object` — and a member typed `java.lang.Object` would let
`apply.rs` exclude `log(String)` at `log(names.get(0))`, a wrong pick. So a
member's types come from its `Signature` attribute whenever it has one, where
`get` returns the type variable `E`, which is recorded as unresolved exactly
as the source reader records it.

## Why asking the build tool is acceptable

It is slow — seconds to minutes — and it may need the network. Neither
matters, because nothing waits for it: the JDK's world is published first,
and a session with no tool on `PATH`, no build file, or a failed resolution
answers exactly as Phase 3b did.

**It executes the project's code.** A Gradle build script is a program, a
Maven build runs plugins, and a wrapper is a script the repository ships. So
it is **off by default**: `build.importOnStartup: true` turns it on for a
project the user trusts. Javelin is a tool for reading code, much of it not
the reader's own, and opening a checkout must not execute it unasked.
IntelliJ asks "Trust project?" before importing for the same reason; an
editor extension that knows about workspace trust can map it to this switch.

## Soundness of the world

`apply.rs` has two arms that assume an external fact cannot lead back into
the open buffer. **A JAR's facts can**: `dependency:build-classpath` resolves
a sibling module from `~/.m2` once it has been installed, so a stale compiled
copy of the workspace's own module is on the classpath, and a workspace that
copies a dependency's class into `src/` to patch it shadows a JAR type with a
source one. Either lets a JAR fact's supertypes name a type the buffer can
reach, and an arm that believed the JAR would exclude a candidate Java
picks.

So a JAR's facts answer names and members and **never subtyping**:
`TypeWorld::extended` lays them after the JDK's, and `TypeWorld::is_subtype`
answers from the platform's facts alone — a JAR type is exactly as unknown
there as a name the world never held, which is what it was before this
phase. What that costs is exclusion: an argument of a JAR type excludes no
overload. A missing answer rather than a wrong one, and the upgrade — a
subtyping question that knows which facts are shadowed by the workspace — is
the one the workspace half of the index already waits on.

## Alternatives rejected

**Reimplementing Maven resolution** (reading `pom.xml`, walking `~/.m2`).
Mediation, BOMs, profiles and property interpolation are a project of their
own, and a classpath that is subtly wrong is the hardest kind to debug.

**The Gradle Tooling API.** A large JVM-side dependency for one question an
init script answers.

**Descriptor-only reading.** Smaller, and unsound for the reason above.
