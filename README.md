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

Every field of `config.yaml` is described in the
[configuration reference](docs/config.md). To run `twcore` on a Linux server
and manage it from the desktop app, see
[Running core on a server](docs/server.md).

## What it does

Point a client (Claude Code, Codex, and friends) at a local port, and:

- **Route by rule** to different upstreams — conditions can be the model name,
  the client, context length, whether tools are present; actions are switching
  upstream, rewriting parameters, or refusing outright.
- **Fail over mid-flight** — before the first byte an upstream can be swapped
  transparently; after it, the only honest thing left is to report what happened.
- **Make cost visible** — token usage, cache hits, priced against a public
  price table that refreshes daily, with price sheets for upstreams that
  charge differently. What cannot be priced is labelled *unknown* rather
  than given an invented number.
- **Redact outbound** — secrets in a request are replaced with placeholders
  before they reach a relay, and restored when the model echoes them back.
- **Inspect inbound** — tool calls returned by an upstream are checked against
  a rule set; a dangerous one can be cut off mid-frame.

## Crate layers

```
tw-dialect · tw-guard · tw-breaker                                   ← shared with the server edition
tw-types · tw-engine · tw-pricing · tw-yaml · tw-secret · tw-watch   ← domain logic
tw-config · tw-store · tw-observe                                    ← assembly
tw-gateway · tw-control · tw-link                                    ← data plane / control plane
```

The server edition depends on the top layer and nothing else: format
conversion and usage parsing (tw-dialect), redaction and tool-call inspection
(tw-guard), and the circuit-breaker state machine (tw-breaker). Those three
depend only on each other — a test enforces it, and CI builds the server
edition against every change to them. A component only one side uses lives
on that side, not here.

Adopting AI clients (pointing their configuration at the gateway), editing
their MCP servers and scanning their configuration live in the desktop app:
they change files on the machine the app runs on, which need not be the one
running core. Core only issues a client its own gateway key.

Everything below is the single-machine implementation (SQLite, unix socket)
and is deliberately **not** shared: single-machine SQLite and multi-tenant
Postgres are different enough that forcing one abstraction over both would
serve neither.

## Development

```
cargo test --workspace     # unit and integration tests
scripts/smoke.sh           # from a clean slate, exercise every path on the real binary
```

`scripts/smoke.sh` touches nothing of yours — `HOME` and `THINKWATCH_HOME` both
point at a temporary directory that is deleted when it finishes.

## The control plane

The control plane listens on a unix socket (a loopback port on Windows).
Every connection starts with a handshake —
`Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`, implemented in `tw-link` — keyed by
`listen.control.key` in `config.yaml`. `twcore serve` writes that key before it
starts listening (into a new configuration, or as one added line in an existing
one). HTTP runs inside the encrypted channel, so curl cannot talk to it;
`twcore call` can:

```
twcore control-key              # print the key the desktop app connects with
twcore control-key --rotate     # replace it; connections made with the old key are closed
twcore call /status
twcore call -X POST -d '{"model":"claude-sonnet-4-5","route":"default"}' /dryrun
```

The configuration text the control plane hands out has the key masked, and a
write through the control plane cannot change it.

A desktop app on another machine connects through the remote control port,
`listen.control.remote`. It is opened in addition to the local channel, with the
same key and handshake:

```
twcore remote enable --allow 192.168.1.0/24   # the port is picked at random the first time
twcore remote disable
twcore control-key                             # prints the key; the address and port go to stderr
```

Sources outside `allow_from` are closed without a reply, a source that fails the
handshake five times in a minute is ignored for a minute, and a remote connection
cannot stop the core, take the diagnostic bundle, or change `listen.control`.

## License

MIT
