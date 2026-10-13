# Javelin for VS Code

Skeleton. Not packaged, not published, not built in CI.

```text
VS Code
 └── Javelin LSP server
```

The extension launches `javelin serve` over stdio and passes the user's settings
through as `initializationOptions`. It contains no Java logic and should stay
that way.

## Trying it locally

1. `cargo build --release` in the repository root and put `target/release/javelin`
   on your `PATH` (or set `javelin.serverPath`).
2. `npm install && npm run compile` in this directory.
3. Open this directory in VS Code and press F5.

## Not done yet

- packaging (`vsce`), publishing, and a CI job
- a bundled server binary, so users do not need a Rust toolchain
