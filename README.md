<p align="center">
  <img src="https://img.shields.io/badge/Rust-000000?style=for-the-badge&logo=rust&logoColor=white" />
  <img src="https://img.shields.io/badge/License-MIT-750014?style=for-the-badge" />
  <img src="https://img.shields.io/badge/macOS-000000?style=for-the-badge&logo=apple&logoColor=white" />
</p>

# ThinkWatch Core

**[English](README.md) | [中文](README.zh-CN.md)**

**The shared core of a local AI API gateway.** Routing, forwarding, observability,
cost accounting, and a set of data-plane guards — used by both the desktop app
(ThinkWatch Lite) and the server edition.

**This repository is not an application you install.** It is a set of crates.
If you want something that runs, `bin/twcore` is a complete, self-contained
gateway binary.

```
cargo run -p twcore -- init     # write a commented config.yaml
cargo run -p twcore -- check    # validate only, don't start
cargo run -p twcore -- serve    # start the gateway and control plane
```

## What it does

Point a client (Claude Code, Codex, and friends) at a local port, and:

- **Route by rule** to different upstreams — conditions can be the model name,
  the client, context length, whether tools are present; actions are switching
  upstream, rewriting parameters, or refusing outright.
- **Fail over mid-flight** — before the first byte an upstream can be swapped
  transparently; after it, the only honest thing left is to report what happened.
- **Make cost visible** — token usage, cache hits, priced against a snapshot
  table. What cannot be priced is labelled *unknown* rather than given an
  invented number.
- **Redact outbound** — secrets in a request are replaced with placeholders
  before they reach a relay, and restored when the model echoes them back.
- **Inspect inbound** — tool calls returned by an upstream are checked against
  a rule set; a dangerous one can be cut off mid-frame.

## Crate layers

```
tw-types · tw-protocol · tw-provider · tw-resil · tw-crypto   ← shape fixed by the outside world
tw-engine · tw-pricing · tw-redact · tw-yaml · tw-secret      ← domain logic
tw-config · tw-store · tw-scan · tw-adopt · tw-observe        ← assembly
tw-gateway · tw-control                                       ← data plane / control plane
```

The top two layers are stable against external reality — the server edition
depends on them directly. The bottom two are the single-machine implementation
(SQLite, unix socket) and are deliberately **not** shared: single-machine SQLite
and multi-tenant Postgres are different enough that forcing one abstraction over
both would serve neither.

## Development

```
cargo test --workspace     # unit and integration tests
scripts/smoke.sh           # from a clean slate, exercise every path on the real binary
```

`scripts/smoke.sh` touches nothing of yours — `HOME` and `THINKWATCH_HOME` both
point at a temporary directory that is deleted when it finishes.

## License

MIT
