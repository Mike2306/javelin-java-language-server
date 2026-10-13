# 0002 — Persistence: SQLite

**Status:** accepted, 2026-09-01

## Context

The workspace index must survive restarts — that is what makes warm startup
"hundreds of milliseconds" instead of a full re-index. It is written by
background workers, read from the request path, and can be killed at any moment
when the editor closes.

Options considered: a custom on-disk format, an embedded key-value store
(`redb`, `sled`, `fjall`), and SQLite via `rusqlite`.

## Decision

SQLite, with WAL journalling and `synchronous = NORMAL`.

## Why

**Crash safety without writing it.** A language server gets killed mid-write
routinely. SQLite's durability story is the most tested one available.

**WAL suits the access pattern.** Background writers do not block readers on the
request path, which is exactly the interactive-latency property the whole
project is organised around.

**Queries the index will need already exist.** Prefix search for workspace
symbols, joins from reference to definition, incremental deletes per file. A
key-value store means writing those by hand.

**Operationally boring.** One file, inspectable with the `sqlite3` CLI, no daemon.

**Schema versioning is trivial.** `user_version` plus a rebuild on mismatch. The
index is derived from source that is still on disk, so discarding it is always
correct and always cheaper than maintaining migrations.

## Costs

`rusqlite` with the `bundled` feature compiles SQLite from source, which adds
noticeably to a clean build. Accepted: it removes any dependency on the system
SQLite, which differs across the three platforms CI targets.

SQLite is not the fastest possible store for bulk symbol writes. If indexing
throughput becomes the bottleneck — measured, not assumed — the fix is batching
inside transactions long before it is a different engine.

## Revisit if

Benchmarks show index writes dominating a cold index build after batching, or
the query surface turns out to be pure key-value after all.
