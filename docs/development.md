# Development

## Requirements

- Rust 1.97.0 — pinned in `rust-toolchain.toml`; rustup installs it on first use
- A C compiler, for the bundled SQLite and the Tree-sitter grammar
- Optional: [`hyperfine`](https://github.com/sharkdp/hyperfine) for startup measurements

## Everyday commands

```bash
cargo c            # check the whole workspace, all targets
cargo t            # test the whole workspace
cargo lint         # clippy, warnings are errors
cargo fmt --all
scripts/check.sh   # the full gate: fmt, clippy, build, test, docs
```

`scripts/check.sh` runs CI's steps in CI's order, plus one CI does not run —
recording the editor-pass numbers — and minus one CI does,
`cargo bench -p javelin-benchmarks --no-run`.

## Running the server

```bash
cargo build --release
./target/release/javelin serve            # what an editor launches
./target/release/javelin doctor .         # cache paths, build system, threads
./target/release/javelin parse Foo.java   # parse one file and report timing
```

Logging goes to stderr, never stdout — stdout is the LSP transport and a stray
write corrupts the protocol stream.

```bash
JAVELIN_LOG=javelin_server=debug ./target/release/javelin serve
javelin --log-level debug --log-file /tmp/javelin.log serve
```

`JAVELIN_LOG` uses `RUST_LOG` syntax and overrides the configured level.

## Layout

```text
crates/          the workspace (see docs/architecture.md for what each one owns)
extensions/      thin Zed and VS Code integrations; skeletons only
tests/           cross-crate and LSP-level integration tests, plus fixtures
benchmarks/      Criterion benchmarks and the shared harness
docs/            this documentation, including ADRs
scripts/         check, bench, bench-startup
```

## Testing

Three levels, and each has a home:

**Unit tests** live in the crate they test, in a `#[cfg(test)] mod tests`. Test
behaviour, not implementation: `positions_past_the_end_clamp_instead_of_panicking`
says what breaks if it fails, `test_offset_2` does not.

**Integration tests** live in `tests/integration/` and exercise real crate
boundaries — a genuine LSP handshake over an in-memory connection, the
document→parser pipeline on real Java files. Anything that only touches one crate
belongs in that crate.

**Process-level tests** live in `crates/cli/tests/`. They spawn the real binary
and talk to it over real pipes, and they catch two things no in-process test
can. The in-memory connection the integration tests use has no I/O threads, so
a shutdown deadlock passes every one of them and then hangs the user's editor —
one such bug was found this way during scaffolding. And a feature the handshake
does not advertise is one no editor will ever ask for, however well its handler
works; that shipped too, in Phase 2a.

**The editor pass** lives there as well: `cargo test -p javelin --test
editor_pass` and `--test reachability`. It used to be a two-minute walk through
Zed at the end of every phase and it is part of the gate now, so
[`manual-testing-zed.md`](manual-testing-zed.md) is the record of *why* each
check exists — and of the three things automating it stopped proving.

**Fixtures** live in `tests/fixtures/`. Keep each one focused on a single
property so a failure names its cause.

New functionality needs tests. Performance-sensitive functionality needs a
benchmark as well.

## Adding a dependency

The startup budget is 50 ms, so dependencies are not free. Before adding one:

1. Can the standard library do it? Use it.
2. Can an already-present dependency do it? Use that.
3. Is it a few lines? Write the few lines.
4. Otherwise: check what it costs at startup, how many transitive crates it
   drags in, and whether it is maintained. Record non-obvious choices as an ADR
   in `docs/adr/`.

## Conventions

- Rust 2024, `rustfmt.toml`, 100-column lines.
- `unsafe` is denied workspace-wide.
- No global mutable state. Thread state explicitly.
- No `println!` for debugging — use `tracing`, and never write to stdout from
  the server.
- Comments explain *why*. The code already says what.
- A deliberate shortcut with a known ceiling gets a `ponytail:` comment naming
  the ceiling and the upgrade path, so it stays findable instead of rotting.
