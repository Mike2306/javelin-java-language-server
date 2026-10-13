# 0001 — LSP library: `lsp-server` + `lsp-types`

**Status:** accepted, 2026-09-01

## Context

The protocol layer should not be written by hand: JSON-RPC framing, the
initialize handshake, and shutdown/exit ordering are all fiddly and all solved.
The two realistic options in Rust are `tower-lsp` and the pair
`lsp-server` + `lsp-types`.

`tower-lsp` is a framework: it owns the event loop, is async, and requires Tokio.
`lsp-server` is a library: it gives a connection with a channel of messages and
leaves the loop to the caller. `lsp-types` provides the protocol structs and is
used by both.

## Decision

Use `lsp-server` and `lsp-types`.

## Why

**Startup cost.** The budget is 50 ms from exec to ready. Tokio's runtime is not
free to start and drags in a substantial dependency tree; nothing in a language
server is I/O-bound in the way an async runtime is designed for. The work is
CPU-bound analysis over a single stdio pipe.

**Scheduling control.** Javelin's central requirement is that interactive
requests never queue behind background work. That is a scheduling policy, and it
needs to live in code we own. Owning the message loop makes the policy explicit;
handing the loop to a framework makes it something to work around.

**Precedent.** rust-analyzer solves the same problem shape with the same pair,
which is also why `lsp-server` is maintained.

## Costs

More code: the message loop, dispatch, and cancellation tracking are ours to
write. That is roughly two hundred lines in `crates/server`, and it is the part
of the server most likely to need project-specific behaviour anyway.

`lsp-types` lags the specification slightly. It has always been close enough.

## Revisit if

We need server-initiated requests with complex concurrency, or the hand-written
loop starts accumulating framework-shaped code.
