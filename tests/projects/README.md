# Workspace fixtures

Whole project trees, used once workspace-level behaviour exists: build-system
detection, multi-module layouts, generated sources.

Each project should be the smallest tree that reproduces the layout under test —
a `pom.xml` and two files beats a copy of a real repository.

- `zed-demo/` — the fixture for the editor pass in
  [`docs/manual-testing-zed.md`](../../docs/manual-testing-zed.md). Covers
  generics, lambdas, records, nested types, a shadowed local, non-ASCII content,
  and one deliberately unparseable file. Its `.zed/settings.json` is gitignored
  because it needs an absolute path to your build.
