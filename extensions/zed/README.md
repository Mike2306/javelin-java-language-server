# Javelin for Zed

Installed as a dev extension from this directory. Not published.

```text
Zed
 ├── Tree-sitter syntax highlighting  (Zed's Java extension)
 └── Javelin LSP server               (this extension)
```

This extension contributes a language *server*, nothing else. It does not define
the Java language or ship a grammar — Zed's Java extension already does both, and
defining a second `Java` language here would collide with it. Install that
extension alongside this one.

What the extension does, all of it: find the `javelin` binary
(`lsp.javelin.binary.path`, or `javelin` on `PATH`), launch it with
`lsp.javelin.binary.arguments` (default `serve`) and the worktree's shell
environment plus `lsp.javelin.binary.env`, and pass
`lsp.javelin.initialization_options` to the server untouched. Any behaviour that
feels like Java knowledge belongs in the server, so Zed and VS Code cannot drift
apart.

## Installing

The steps, and the settings that matter — above all
`build.importOnStartup` — are the [README's quick start](../../README.md#quick-start-in-zed).
In short: `cargo build --release` at the repository root, `rustup target add
wasm32-wasip2`, then `zed: install dev extension` and select this directory.

## Checking it works

- `zed: open log` shows the handshake; `"arguments": ["serve", "--log-file",
  "/tmp/javelin.log"]` captures Javelin's own structured logs, where
  `classpath ready` and `workspace ready` say when the JDK and the index have
  landed.
- The editor pass is automated — `cargo test -p javelin --test editor_pass`
  drives this same binary over real pipes. What only a human in Zed can check is
  the walk in [`docs/manual-testing-zed.md`](../../docs/manual-testing-zed.md).

## Notes

- This directory is a separate Cargo workspace. Zed extensions compile to
  `wasm32-wasip2`, which would otherwise force that target on every crate.
- After changing the extension source, rebuild it from the extensions page
  (or reinstall the dev extension). After
  rebuilding the *server*, restart the language server (`editor: restart
  language server`) — the extension does not need rebuilding.
- Answers improve while you work: the JDK's sources are read on a background
  thread after the handshake — about eleven seconds on the first session on a
  machine, a tenth of a second from the cache after that — and no request ever
  waits on it.
