# 0003 — Parser: Tree-sitter

**Status:** accepted, 2026-09-01

## Context

The interactive layer needs a parser that runs on every keystroke and produces
something usable from source that is, by definition, usually incomplete.

## Decision

Tree-sitter with the `tree-sitter-java` grammar, wrapped in `javelin-parser` and
exposed nowhere else.

## Why

**Error recovery is the default, not a mode.** A file being typed is a file with
syntax errors. Tree-sitter still yields a tree, which is what makes outline and
local completion work mid-keystroke.

**Incremental reparsing.** Given the edits, it reuses unchanged subtrees. That is
the difference between a parse that scales with the edit and one that scales with
the file.

**Concrete syntax.** The tree keeps every token and every byte range, which
matters for hover ranges, semantic tokens, and refactorings.

## Costs

Tree-sitter is a *syntax* layer and nothing more. Types, scopes, and resolution
are `javelin-semantic`'s problem, and that separation has to be maintained
deliberately — it is tempting to answer semantic questions with syntactic
heuristics because the tree is right there.

The grammar is C and is compiled from source.

## Current limitation

Reparsing is not yet incremental. `JavaParser::parse` re-scans the whole file,
because the document store does not yet report which byte ranges changed. The
reuse path is deliberately absent rather than approximated: handing Tree-sitter
a previous tree without matching `InputEdit`s silently produces a *wrong* tree.
Threading edits through is the first task of phase 1.

## Revisit if

Java semantics eventually need a full compiler front end. Even then Tree-sitter
likely stays as the interactive layer, with the heavier analysis behind it.
