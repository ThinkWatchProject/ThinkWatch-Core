<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/brand/logo-dark.png">
    <img src="docs/brand/logo.png" alt="ThinkWatch Core" width="580">
  </picture>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/Rust-000000?style=for-the-badge&logo=rust&logoColor=white" alt="Rust" />
  <img src="https://img.shields.io/badge/License-MIT-750014?style=for-the-badge" alt="License: MIT" />
  <img src="https://img.shields.io/badge/arm64-555555?style=for-the-badge&label=macOS&labelColor=000000&logo=apple&logoColor=white" alt="macOS: arm64" />
  <img src="https://img.shields.io/badge/x64%20%7C%20arm64-555555?style=for-the-badge&label=Windows&labelColor=0078D4" alt="Windows: x64, arm64" />
  <img src="https://img.shields.io/badge/x86__64%20%7C%20aarch64-555555?style=for-the-badge&label=Linux&labelColor=FCC624&logo=linux&logoColor=black" alt="Linux: x86_64, aarch64" />
</p>

# ThinkWatch Core

**[English](README.md) | [中文](README.zh-CN.md)**

ThinkWatch Core is the gateway engine behind ThinkWatch: a set of Rust crates
and the `twcore` binary built from them. Claude Code, Codex and other clients of
the Anthropic, OpenAI and Gemini APIs point at `twcore` once. From then on,
credentials can be kept out of outgoing requests, the tool calls that come back
are checked, and every request is recorded with where it went and what it cost. `twcore` runs inside the
desktop app [ThinkWatch Lite](https://github.com/ThinkWatchProject/ThinkWatch-Lite)
or on its own on a Linux server, and
[ThinkWatch Enterprise](https://github.com/ThinkWatchProject/ThinkWatch) builds
on four of its crates.

Documentation: [configuration reference](docs/config.md) ·
[running core on a server](docs/server.md) ·
[thinkwat.ch/core](https://thinkwat.ch/core/)

## Highlights

- **Connect once, switch freely.** Clients keep one address and one key;
  changing upstreams or models happens in the gateway, with no client change or
  restart. Anthropic Messages, OpenAI Chat Completions, OpenAI Responses and
  Gemini are converted in both directions, streams included.
- **Outbound redaction.** Outbound redaction can replace API keys,
  private keys and connection-string passwords with placeholders before a
  request leaves, and restore them where the answer repeats them, so a relay
  never sees the real values.
- **Malicious tool calls are cut off.** A relay can rewrite an answer and slip
  in a tool call for the client to run. Tool-call inspection can cut off an
  answer whose tool call downloads and runs code, sends out environment
  variables or credential files, reads private keys, or installs a startup item
  or scheduled job, before the client receives it whole; hidden-character
  detection, a content filter and an output limit complete the five
  protections. All start
  in observe mode (the output limit starts off) and change nothing until set to
  enforce.
- **Every request is traceable.** Each request is stored with the rule that
  chose its upstream, every attempt, any format conversion, usage, cost and
  where its price came from, time to first token and generation speed. A dry
  run shows where a request would go without sending it, and a stored request
  can be replayed against another upstream for comparison.
- **Routing and failover.** Rules match on model, key, format, size, tools,
  images, thinking and more, and send requests to an upstream or a group (in
  order, manual, rotation, lowest latency, lowest cost). Until the first byte
  of the response reaches the client, a failing upstream is replaced by the
  next candidate and set aside for a time that depends on the reason it gives.
- **Many kinds of upstream.** Provider API keys, any compatible endpoint, relays
  such as OpenRouter, local models, Amazon Bedrock, and ChatGPT or Z.ai
  accounts. Health checks and warm-ups are answered locally by default, at no cost.
- **Honest cost.** Usage, cache reads and writes included, is priced from a
  daily-refreshed public price table or a custom price sheet. Estimated costs
  are marked as such, and usage that cannot be priced is counted separately,
  never as zero.
- **One file, applied live.** All settings live in `config.yaml`; a change from
  an editor, the CLI or the control plane applies within a second once it
  validates, and the last fifty versions can be restored.

## Install

**Desktop.** [ThinkWatch Lite](https://github.com/ThinkWatchProject/ThinkWatch-Lite)
includes `twcore` and updates it with the app; nothing else to download.

**Linux server** (x86_64 or aarch64, glibc 2.35+: Ubuntu 22.04, Debian 12 or
later). One command installs `twcore` as a systemd service with its own user:

```sh
curl -fsSL https://raw.githubusercontent.com/ThinkWatchProject/ThinkWatch-Core/main/scripts/install.sh | sudo sh
```

The script does not start the service. Then, as the service user:

```sh
alias twc='sudo -u thinkwatch THINKWATCH_HOME=/var/lib/thinkwatch twcore'

twc remote enable --allow 192.168.1.0/24   # open the remote control port to this network
twc check                                  # validate config.yaml
sudo systemctl enable --now twcore         # start the service, now and at boot
twc control-key                            # the key for ThinkWatch Lite on stdout; address and port on stderr
```

In ThinkWatch Lite, **Settings → Connection → Add remote connection** takes the
address, control port and key. The app connects only to the core version it
includes; `sudo twcore upgrade --version <version> --restart` switches the
server to it. Neither port uses TLS, so keep both on trusted networks.
[Running core on a server](docs/server.md) covers secrets, network exposure,
upgrades and uninstalling.

**Prebuilt binaries** for macOS (Apple silicon), Windows (x64, ARM64) and Linux
(x86_64, aarch64) are attached to every
[release](https://github.com/ThinkWatchProject/ThinkWatch-Core/releases/latest),
each with a `.sha256` checksum; the Linux `.tar.gz` includes the systemd unit.

`twcore` keeps its configuration and data in `~/.thinkwatch`
(`%APPDATA%\ThinkWatch` on Windows), or in `THINKWATCH_HOME`. Every field is in
the [configuration reference](docs/config.md).

## Control plane

The control plane is an HTTP API inside an encrypted channel: a unix socket
(a loopback port on Windows), plus the optional remote port. Every connection
starts with a `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s` handshake keyed by
`listen.control.key`; there are no certificates. curl cannot reach it;
`twcore call` can:

```
twcore call /status
twcore call -X POST -d '{"model":"claude-sonnet-4-5","route":"default"}' /dryrun
twcore control-key --rotate     # replace the key; connections made with the old key are closed
```

The remote port admits only sources in `allow_from`, at most 32 connections at
a time, and cannot stop core, take the diagnostic bundle or change
`listen.control`.

## Crates

| Crate | Role |
|---|---|
| `tw-dialect` | Conversion between the four API formats; usage parsing |
| `tw-guard` | The five protections: redaction, tool-call inspection, hidden characters, content filter, output limit |
| `tw-breaker` | Circuit-breaker state machine |
| `tw-bedrock` | Amazon Bedrock on the wire: SigV4 signing, eventstream, addresses, model catalog |
| `tw-types` | Messages for people: stable code, arguments, English sentence |
| `tw-engine` | Routing rules and groups |
| `tw-pricing` | Price table, price sheets, measured / estimated / unpriced cost |
| `tw-yaml` | Minimal edits to the original YAML text |
| `tw-secret` | Environment and command-sourced credentials, masking |
| `tw-watch` | Debounced directory watching |
| `tw-api` | Control-plane contract: types and client |
| `tw-link` | Control-channel handshake and encryption |
| `tw-config` | Configuration schema, loading and validation |
| `tw-store` | Request history and runtime state on SQLite |
| `tw-observe` | Event bus |
| `tw-gateway` | Data plane: the life of a request |
| `tw-plugin` | Script-plugin sandbox: QuickJS compiled to WebAssembly, run by Wasmtime |
| `tw-control` | Control-plane server |

ThinkWatch Enterprise depends only on the first four, which depend only on one
another; CI checks that Enterprise compiles against every change to them.
ThinkWatch Lite pins `tw-api`, `tw-types`, `tw-yaml`, `tw-guard`, `tw-watch` and
`tw-link` to a release tag and bundles the `twcore` of the same release. Setting
up AI clients and scanning their configuration happen in Lite, on the machine
it runs on; `twcore` issues each client its own gateway key. The binary lives
in `bin/twcore`. Only `tw-gateway` and `twcore` may depend on `tw-plugin`, the
one crate whose build needs more than Rust (see below), so building Lite or
Enterprise against these crates never does.

## Build and test

Requires a recent stable Rust toolchain (1.94.1 or later), plus an LLVM `clang`
that can compile C to WebAssembly and the `llvm-ar` that comes with it. The
plugin sandbox (`tw-plugin`) compiles QuickJS to WebAssembly while it builds;
Apple's clang cannot target WebAssembly.

| System | Install |
|---|---|
| macOS | `brew install llvm` (found where Homebrew puts it; it does not need to be on `PATH`) |
| Debian, Ubuntu | `sudo apt install clang llvm` |
| Fedora | `sudo dnf install clang llvm` |
| Windows | the LLVM installer from [LLVM's releases](https://github.com/llvm/llvm-project/releases), or `winget install LLVM.LLVM` |

The build tries Homebrew's LLVM, then `clang` and `clang-N` (`clang-19`,
`clang-18`, …) on `PATH`, and uses the first that really produces WebAssembly.
To pick one yourself, set `TW_WASM_CLANG`, and `TW_WASM_AR` when its `llvm-ar`
is not next to it. Rust's `wasm32-unknown-unknown` target is listed in
`rust-toolchain.toml`, so rustup installs it; the linking is done by the
`rust-lld` that ships with Rust. Nothing is downloaded during the build. Each
build records which clang it used and the SHA-256 of the WebAssembly module
(`tw_plugin::GUEST_CLANG`, `tw_plugin::GUEST_WASM_SHA256`).

```sh
cargo build --release -p twcore     # target/release/twcore
cargo run -p twcore -- init         # write an initial config.yaml
cargo run -p twcore -- serve        # start the gateway and the control plane
cargo test --workspace              # unit and integration tests
scripts/smoke.sh                    # every path on the real binary, in a temporary HOME
```

[CONTRIBUTING.md](CONTRIBUTING.md) lists the checks a pull request has to pass
and describes the configuration reference, the price list and releases.

## License

[MIT](LICENSE)
