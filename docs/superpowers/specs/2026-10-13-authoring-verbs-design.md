# Authoring verbs: rename, code actions, auto-import, formatting

Date: 2026-10-13. Status: decided (project lead), being built in the order below.

## Why, and what changes in the product

The user asked for the four verbs the roadmap held out of version 1. The
roadmap's reasons were "a tool that writes to the project is a different set of
promises" and, for formatting, "a JVM on the startup path". Neither forbids the
verbs. They set terms for them:

- **A write that is wrong costs more than a read that is wrong.** A missing
  reference is a ceiling; a missing reference *in a rename* is a broken
  build. So every verb here answers only where it can prove the edit is whole,
  and refuses everywhere else. "Ambiguity answers nothing" applies with more
  force, not less.
- **Nothing here touches startup.** No verb runs before it is asked. The
  formatter is a process the user names, spawned only on request.

## Slice 1: rename (`textDocument/prepareRename`, `textDocument/rename`)

References are best-effort today (recorded refs miss a file's own bare-field
uses, overrides that fix a type argument, files still mid-index, and a
byte-offset key that goes stale after an edit). That is acceptable for reading
and unacceptable for a rename. So slice 1 renames only targets whose every use
must be in the open file:

- **Variables**: locals, parameters, pattern bindings. Java's scoping confines
  them to their binding's scope.
- **Private fields and methods** of a type in this file. Java confines private
  access to the top-level type, so to this file.

Everything else (types, non-private members, constructors, type parameters,
local classes, record components, enum constants) is refused.
`ponytail:` the upgrade is a published "references complete" signal per
member plus a fix for each recorded-refs gap; then non-private members and
types can follow.

**Uses.** Every identifier token with the old spelling in the search range: the
scope for a variable, the whole file for a member. Each token is resolved with
`definition_in`, the rule references already uses:

- If it lands on the target, it is renamed.
- If it lands on another declaration, it is left alone.
- If it lands on nothing **and could be the target**, the rename is refused.

For a variable, "could be" means an expression name (`parser::is_expression_name`).
A member selection, method name or type cannot be a variable. For a member,
every token counts.

**Refusals.** The rename is refused when any of the following holds:

- The file has parse errors.
- The open view is older than the buffer (a reparse is in flight).
- For a member:
  - the file mentions `lombok`, because generated accessors are uses written
    under another spelling;
  - the old name appears as a word inside a string literal (`@MethodSource("foo")`
    and reflection);
  - an *unqualified* use sits inside a type other than the declaring one,
    because an inner type's inherited member could capture the new name;
  - the declaring type's member walk is not whole, or it already has a member
    with the new name.

**New name.** It must be a Java identifier, not a keyword or a reserved
contextual word (`var`, `yield`, `record`, `_`). It is also refused when any
token spelled like the new name appears in the search range: for a member,
that range is the whole file. For a variable, the new name must also bind
nothing visible at the declaration. This is coarse, and sound: no capture is
possible when the new name is never written where it could be captured.

**Answer.** prepareRename returns the name's range and the old name as the
placeholder, or `null` when refused. Rename returns a `WorkspaceEdit` with one
`changes` entry for this file, or a `RequestFailed` error naming the reason.
`renameProvider: { prepareProvider: true }`.

## Slice 2: add-import quick fix (`textDocument/codeAction`, quickfix)

This applies to each `unresolved-type` diagnostic in the request's context. The
name is read from the buffer at the diagnostic's range. Candidates are every
type with that simple name that meets all of these:

- the JDK/JAR world (`TypeWorld`, via a new `by_simple` index) or the
  workspace index (`with_simple_name`) holds it;
- it is **public**, along with every outer type it is nested in. This needs a
  new `TypeFacts::is_public` and `WorkspaceType::is_public`, and so a
  schema bump;
- it is, if a JDK type, in a package under `java.`, `javax.`, `org.w3c.`, `org.xml.` or `org.ietf.` and not an AWT `peer` package — exactly the packages JDK 21 exports under those prefixes;
  `ponytail:` read `module-info` exports instead;
- its module scope sees it (`Scope::sees`) when the file has one.

Each candidate gives one quick fix, "Import `fqn`", sorted by fqn. The user
picks, so several candidates are not a guess. None is marked preferred unless
there is exactly one. The edit inserts `import fqn;`:

- among the single-type imports, before the first one that sorts after it;
- after the last import when there is none;
- after the package declaration, preceded by a blank line, when the file has no
  imports;
- at the top of the file when there is no package.

Auto-import *on completion* stays out. Completion is still not invested in;
its existing `ponytail:` names what it would take.

## Slice 3: remove unused imports (`source.organizeImports`)

A single-type, non-static import is unused when its simple name is written
nowhere else in the file as a whole word: not as a name, not in a comment
(`{@link Foo}`), not even in a string, which keeps an import that could have
gone rather than read the tree for it. Static and on-demand imports are never
touched. A file with parse errors, or with a `\u` escape that could spell a
name, gets no action. The action is titled "Remove unused imports", so a
client's organize-on-save can run it. It deletes each unused import's whole
line, and leaves an import that shares its line with anything else.

## Slice 4: formatting (`textDocument/formatting`)

Configured with `format.command`, an argv array such as
`["google-java-format", "-"]`. It is unset by default, and with no command the
capability is not advertised. The buffer is written to stdin; stdout is the
formatted file. A non-zero exit, a timeout (`format.timeoutMs`, default
10 000), or empty output answers `RequestFailed` with the first stderr line.

The request loop must not wait on a JVM. The router hands the request to a
thread that holds a clone of the connection's sender and answers from there.
The response is one edit that replaces the whole document; Zed and VS Code both
diff a large edit down to minimal changes. `ponytail:` `$/cancelRequest` does
not reach the thread, which the timeout bounds instead.

## Slice 5: workspace rename

Slice 1's `ponytail:` closed by a different route than it named: rather than
trust the recorded member references, the rename reads every source the
workspace index lists, the open buffer where there is one and the disk
otherwise, and resolves every token of the old name with references' own rule.
A text search over every file is a superset, so a missing recorded reference
cannot hide a use.

**Targets.** A top-level type, from its name or a constructor's; a non-private
field, method or enum constant; and a use, in the open file, of a member
another file declares. Locals, parameters and private members stay on slice
1's one-file path.

**Refused before reading a file:** the walk is not whole or the index is
behind the buffers; the declaration is in the JDK or a dependency, is a nested
type, a record component, or something Lombok generates or may generate
accessors for; two workspace files declare the type; the client cannot rename
a file and the type's file is named after it; or the target file already
exists.

**Methods** are renamed with their override family. The family is built from
each owner's member closure (`externals.members`, which must be complete) and
`hierarchy::all_subtypes`: a method of the same name and the same parameter
text joins it, one of the same arity written differently refuses the rename
(it may override, and the text cannot tell), and a family member overriding a
library method or one of `Object`'s refuses it. So does a family member
declared in an anonymous or local class, which the index does not list.

**Uses.** Imports and static imports are matched as text. Every other token of
the old name is resolved: on the target it is renamed, elsewhere it is left,
and nowhere while it could be the target refuses the rename, except an
annotation element key and a receiver `missing_member_at` proves has no member
of that name. A string literal naming the type's qualified name where it is
used, or the member's name in a file that declares it, refuses too.

**New name.** A type's is refused where the workspace already declares it, and
wherever it is written in an edited file, a file of the type's package, or a
file importing that package on demand. A member's is refused where the
hierarchy already has it, and where it is written in a file with a rewritten
bare use or a declaration, or where an anonymous class declares it in a file
naming a type of the hierarchy.

**Answer.** Versioned `documentChanges` where the client takes them: an open
file's edit carries its buffer's version, a closed file's `null`. They are
followed by a `RenameFile` when the file's stem is the type's name. A client
without them gets `changes`, and a type rename that would move its file is
refused. The request runs on a thread of its own, like formatting.

`ponytail:` ceilings, each also in the code:

- Only Java sources are read: a qualified name in XML, a `META-INF/services`
  entry, a properties file or a javadoc `{@link}` is left behind.
- `$/cancelRequest` does not reach the sweep thread; the workspace bounds it.
- Nested types are refused rather than renamed.
- A member type inherited from a supertype in another file or a JAR is not
  checked, so a renamed type's use inside a subclass can be captured by an
  inherited member type of the new name.

## Testing

Each slice gets unit tests in its handler for the refusals as much as for the
answers, and an entry in `crates/cli/tests/reachability.rs`'s `SURFACE`. The
`capabilities.rs` assertion that rename is unadvertised flips. Formatting is
tested with a fake command (`cat`, `false`, a sleeper for the timeout).
`scripts/check.sh` must pass after each slice.

## Docs

The roadmap's version-1 framing, Phase 5b and the README feature table change
to say which authoring verbs exist and what they refuse. The README documents
`format.command`.
