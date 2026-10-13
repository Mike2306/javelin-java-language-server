# Javelin

A Java language server built for one thing: being fast enough that you forget it
is there.

> **Status: first release, for reading Java in Zed.** Outline, definition,
> hover, completion, references, implementations, type and call hierarchy,
> workspace symbols, document highlight, semantic tokens, folding and
> selection ranges — across the workspace's own sources, the JDK's and, with
> the build tool's help, the dependencies' — plus a warning for a type or a
> method that does not exist. Nothing from Phase 3b onwards has yet been
> walked by a human in an editor; the walk is
> [`docs/manual-testing-zed.md`](docs/manual-testing-zed.md). See
> [the roadmap](docs/roadmap.md) for what is deliberately out.

## Quick start in Zed

You need Rust (the toolchain is pinned in `rust-toolchain.toml`; `rustup`
installs it on first build), a C compiler, Zed, and a JDK whose `lib/src.zip`
is present — Temurin, Zulu and Oracle builds ship it.

1. **Build the server** and check what it finds:

   ```bash
   cargo build --release
   ./target/release/javelin doctor .
   ```

   `doctor` names the JDK it finds (`$JAVA_HOME`, then the usual install
   locations — a `jdk.home` set in Zed is not seen by `doctor`) and the
   cache it will use.

2. **Install Zed's Java extension** for highlighting (`zed: extensions`,
   search for *Java*). Javelin contributes a language server, not a grammar.

3. **Install Javelin as a dev extension**: `rustup target add wasm32-wasip2`
   once, then in Zed run `zed: install dev extension` and pick
   [`extensions/zed`](extensions/zed). Zed compiles it itself.

4. **Point Zed at the server and turn the other one off**, in
   `zed: open settings` (or a project's `.zed/settings.json`):

   ```json
   {
     "languages": {
       "Java": { "language_servers": ["javelin", "!jdtls"] }
     },
     "lsp": {
       "javelin": {
         "binary": {
           "path": "/absolute/path/to/javelin-java-language-server/target/release/javelin",
           "arguments": ["serve", "--log-file", "/tmp/javelin.log"]
         },
         "initialization_options": {
           "build": { "importOnStartup": true }
         }
       }
     }
   }
   ```

   Leave out `binary.path` if `javelin` is on your `PATH`. `"!jdtls"` keeps
   the Java extension's own server from answering beside Javelin.

5. **Open a Java project.** The open file answers at once. The JDK is read
   in the background — about eleven seconds the first time on a machine, a
   tenth of a second from the cache afterwards — and answers improve when it
   lands; nothing waits for it.

**The setting that matters: `build.importOnStartup`.** In a Maven or Gradle
project, dependency types resolve **only** with `"importOnStartup": true`:
Javelin then asks the build tool for the classpath (`mvn
dependency:build-classpath`, or a Gradle init script) on a background thread.
**That runs the project's build** — its wrapper, its build script, its plugins
— so it is off by default and should only be turned on for a project you
trust. With it off, a Maven or Gradle project is navigable through its own
sources and the JDK, dependency types answer nothing, and **no diagnostics
are published at all**, because every dependency type would be flagged.

With it on, Javelin also asks the build tool once per classpath for each
dependency's `-sources.jar` (`mvn dependency:sources`, or a Gradle init
script; then `mvn` on `PATH` for a JAR Gradle took from `mavenLocal()`), so a
library member jumps and hovers with parameter names and Javadoc. That
downloads from the build's repositories into its usual caches;
`"build": { "downloadSources": false }` turns it off.

Other settings, all under `initialization_options`, camelCase: `jdk.home` (a
JDK other than the discovered one), `index.cacheDir`,
`index.maxFileSizeBytes` (lowering it below the 2 MiB default turns
diagnostics off, since a skipped file could declare any name),
`diagnostics.enabled` and `diagnostics.debounceMs`. A misspelt key is
ignored silently, and a value of the wrong type resets *every* setting to its
default with only a log warning — check the log after changing them. The log
level is a command-line flag: `"arguments": ["serve", "--log-level",
"debug"]`.

## Why

Existing Java language servers ask you to wait. The editor opens, and then there
is a period — sometimes seconds, sometimes much longer on a real enterprise
monorepo — during which the file in front of you is just text. The workspace has
to be imported, the build system consulted, the JDK read, the world indexed.
Only then does the editor become an IDE.

Javelin inverts that order. The workspace import still happens; it just happens
behind you.

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

Nothing on that path waits for the step below it. A request answers with what is
known now and gets better as the engine warms up, rather than blocking until it
can be complete.

## Architecture

```text
Editor (Zed, VS Code, any LSP client)
   │
   ▼
LSP layer  ──▶  scheduler  ──┬── document engine
                             ├── parser (Tree-sitter)
                             ├── semantic engine
                             ├── symbol / index layer
                             ├── class / JAR / JDK model
                             ├── build systems (Maven, Gradle)
                             └── persistent cache (SQLite)
```

The server is editor-agnostic. Editor integrations are thin launchers with no
Java knowledge in them, so Zed and VS Code cannot drift apart.

Details in [docs/architecture.md](docs/architecture.md); the reasoning behind the
major dependency choices is in [docs/adr/](docs/adr/).

## What works today

Reading, not writing. What each request answers, and every refusal it makes
on purpose, is in [the roadmap](docs/roadmap.md); the short form:

| | |
|---|---|
| ✅ | Outline, folding, selection ranges, document highlight, semantic tokens — from the open file, instantly |
| ✅ | Definition, type definition, hover, completion — the open file, the workspace's other files, the JDK's members and dependency JARs' members, through a chain of generic calls, lambdas and method references. A JAR member or type jumps into its `-sources.jar`, downloaded through the build tool when it is not on disk |
| ✅ | References across the workspace, following overrides; implementations; type and call hierarchy; workspace symbols |
| ✅ | Warnings for a type, method or field name that resolves to nothing — only where the world is whole enough to be sure |
| ✅ | Parameter-name and type inlay hints, and Javadoc in hover |
| ✅ | Lombok's generated members, and annotation-processor, protobuf, OpenAPI and jOOQ output |
| ✅ | A background workspace index that follows saves, edits and `git checkout`s |
| ⬜ | Rename, code actions, auto-import, formatting — out of the first release |

## Performance philosophy

Performance is a product requirement, so the benchmark harness exists before the
features it measures. Interactive operations are judged at p95, not the mean: a
good average with a bad tail is exactly what "it feels sluggish" means.

| Operation | Target |
|---|---|
| Process startup | < 50 ms |
| `initialize` | < 100 ms |
| First parse | < 10 ms |
| Local completion | < 50 ms |
| Definition | < 50 ms |
| Local references | < 100 ms |
| Warm startup | hundreds of ms |

These are goals, not measurements of a finished system. What has actually been
measured — including the budgets Phase 1 misses, all of them on generated
god-classes — is in [docs/performance.md](docs/performance.md).

## Editors

- **Zed** — [`extensions/zed`](extensions/zed), installed as a dev extension (above)
- **VS Code** — [`extensions/vscode`](extensions/vscode) (skeleton)
- **Anything else that speaks LSP** — run `javelin serve` over stdio

## Building

Rust 1.97 (pinned in `rust-toolchain.toml`) and a C compiler.

```bash
cargo build --release
./target/release/javelin doctor .
```

```bash
cargo c            # check
cargo t            # test
cargo lint         # clippy, warnings are errors
scripts/check.sh   # the full gate, exactly what CI runs
scripts/bench.sh   # benchmarks
```

More in [docs/development.md](docs/development.md).

## Contributing

Two things are expected of any change:

- New functionality comes with tests. Performance-sensitive functionality comes
  with a benchmark.
- `scripts/check.sh` passes.

Read [CLAUDE.md](CLAUDE.md) first. It is written for AI assistants, but it is
also the shortest statement of the rules every change follows and of what this
project will and will not do.

## Licence

MIT. See [LICENSE](LICENSE).
