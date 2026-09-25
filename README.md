<p align="center">
  <img src="https://img.shields.io/badge/Rust-000000?style=for-the-badge&logo=rust&logoColor=white" alt="Rust" />
  <img src="https://img.shields.io/badge/License-MIT-750014?style=for-the-badge" alt="License: MIT" />
  <img src="https://img.shields.io/badge/arm64-555555?style=for-the-badge&label=macOS&labelColor=000000&logo=apple&logoColor=white" alt="macOS: arm64" />
  <img src="https://img.shields.io/badge/x64%20%7C%20arm64-555555?style=for-the-badge&label=Windows&labelColor=0078D4" alt="Windows: x64, arm64" />
  <img src="https://img.shields.io/badge/x86__64%20%7C%20aarch64-555555?style=for-the-badge&label=Linux&labelColor=FCC624&logo=linux&logoColor=black" alt="Linux: x86_64, aarch64" />
</p>

# ThinkWatch Core

**[English](README.md) | [中文](README.zh-CN.md)**

ThinkWatch Core is a set of Rust crates and the `twcore` gateway binary built
from them. Claude Code, Codex and other clients of the Anthropic, OpenAI and
Gemini APIs send their requests to `twcore`, which routes each request by rule,
fails over to another upstream before the response begins, records what each
request cost, redacts secrets before a request is sent, and inspects the tool
calls an upstream returns.

`twcore` runs as the local gateway inside
[ThinkWatch Lite](https://github.com/ThinkWatchProject/ThinkWatch-Lite), the
desktop app for macOS, Windows and Linux, or as a standalone gateway on a Linux
server, which ThinkWatch Lite connects to over an encrypted control channel.
[ThinkWatch Enterprise](https://github.com/ThinkWatchProject/ThinkWatch)
depends on three of the crates: `tw-dialect`, `tw-guard` and `tw-breaker`.

Documentation: [configuration reference](docs/config.md) ·
[running core on a server](docs/server.md) ·
[thinkwat.ch/core](https://thinkwat.ch/core/)

## Features

- **Rule-based routing.** Named rules match on the requested model (glob
  patterns), the gateway key, the API format, the estimated input token count,
  `max_tokens`, and whether a request uses tools, images, extended thinking,
  prompt caching or streaming. A matching rule sends the request to an upstream
  or a group, rewrites parameters (the model, `max_tokens`, thinking), or
  refuses it with a stated reason. A group chooses among its upstreams in order
  (`fallback`), by manual selection (`select`), in rotation (`load-balance`), by
  measured time to first byte (`url-test`) or by price (`cheapest`).
- **Failover.** Until the first byte of a response reaches the client, a failed
  upstream is replaced by the next candidate without the client noticing, and
  every attempt is recorded with the request. A stream that breaks after that
  point ends with an error event, so the client does not take it for a complete
  answer. An upstream that fails three times in a row is set aside for a minute
  before it is tried again.
- **Format conversion.** Clients can use Anthropic Messages, OpenAI Chat
  Completions, OpenAI Responses or Gemini, independently of the format the
  upstream speaks; when the two differ, requests, responses and streams are
  converted. An upstream can be a provider's API, a relay such as OpenRouter, a
  local model server or a ChatGPT account.
- **Cost accounting.** The token usage of every request, cache reads and writes
  included, is priced against a public price table that is refreshed daily, or
  against a price sheet for upstreams that charge differently (a multiplier or
  per-model prices). An upstream such as a local model can be marked as free.
  Usage that cannot be priced is labelled unknown rather than counted as zero,
  and each request records its cost and where the price came from.
- **Protections.** Five protections apply to every request, whatever the
  upstream, each in off, observe or enforce mode. In enforce mode, outbound
  redaction replaces credentials such as API keys, private keys and the
  passwords in connection strings with placeholders before a request is sent,
  and restores them where the response repeats them; tool-call inspection can
  cut the response off when an upstream returns a tool call that matches a rule
  for dangerous commands; hidden-character detection refuses requests that
  carry Unicode tag characters or bidirectional controls; content filtering can
  refuse requests that match a keyword or regular-expression rule; and the
  output limit cuts an answer off at a set number of characters. In observe
  mode a protection records what it finds and changes nothing. Every protection
  starts in observe mode except the output limit, which starts off. Built-in
  rules can be turned off one at a time, and custom rules added.
- **Request history and live events.** Every request is stored in a local
  SQLite database with the rule it matched, each upstream attempt, its usage and
  its cost. The control plane streams events as requests start and finish, and a
  dry run shows where a request would be routed, and why, without sending it.
- **One configuration file.** All settings are kept in `config.yaml`. A change,
  whether made in an editor, with `twcore config` or through the control plane,
  takes effect within a second once it validates; a change that does not
  validate is refused and the previous configuration keeps serving. The last
  fifty versions are kept, and any of them can be restored.

## Install

### Prebuilt binaries

Each [release](https://github.com/ThinkWatchProject/ThinkWatch-Core/releases/latest)
provides `twcore` for five platforms, every file with a `.sha256` checksum:

| Platform | File |
|---|---|
| macOS, Apple silicon | `twcore-aarch64-apple-darwin` |
| Windows, x64 | `twcore-x86_64-pc-windows-msvc.exe` |
| Windows, ARM64 | `twcore-aarch64-pc-windows-msvc.exe` |
| Linux, x86_64 | `twcore-x86_64-unknown-linux-gnu`, or `twcore-x86_64-unknown-linux-gnu.tar.gz` with the systemd unit |
| Linux, aarch64 | `twcore-aarch64-unknown-linux-gnu`, or `twcore-aarch64-unknown-linux-gnu.tar.gz` with the systemd unit |

To verify a download, run `sha256sum -c <file>.sha256` (on macOS,
`shasum -a 256 -c <file>.sha256`) in the directory that holds both files. The
Linux builds require glibc 2.35 or later (Ubuntu 22.04, Debian 12 or later).
A desktop needs no separate download: ThinkWatch Lite includes `twcore` and
updates it together with the app.

### Linux server

```sh
curl -fsSL https://raw.githubusercontent.com/ThinkWatchProject/ThinkWatch-Core/main/scripts/install.sh | sudo sh
```

The script installs `twcore` as a systemd service. The steps that follow are
summarized under [Server deployment](#server-deployment) and described in full
in [Running core on a server](docs/server.md).

### From source

Building requires a stable Rust toolchain, 1.85 or later.

```sh
git clone https://github.com/ThinkWatchProject/ThinkWatch-Core.git
cd ThinkWatch-Core
cargo build --release -p twcore     # produces target/release/twcore
```

To run it from the checkout:

```sh
cargo run -p twcore -- init     # write an initial config.yaml
cargo run -p twcore -- check    # validate the configuration without starting
cargo run -p twcore -- serve    # start the gateway and the control plane
```

`twcore` keeps its configuration and data in `~/.thinkwatch`
(`%APPDATA%\ThinkWatch` on Windows), or in the directory named by
`THINKWATCH_HOME`. Every field of `config.yaml` is described in the
[configuration reference](docs/config.md).

## Server deployment

`twcore` can run as a systemd service on Linux (x86_64, aarch64). A single
command installs the binary, a dedicated user and the service unit; ThinkWatch
Lite on macOS, Windows and Linux connects to it through the remote control
port.

```sh
curl -fsSL https://raw.githubusercontent.com/ThinkWatchProject/ThinkWatch-Core/main/scripts/install.sh | sudo sh
```

The script checks the download against its SHA-256 checksum, installs
`/usr/local/bin/twcore`, creates the system user `thinkwatch` with the data
directory `/var/lib/thinkwatch`, and installs `twcore.service`. When there is no
configuration yet, it runs `twcore init` as that user, which writes a
`config.yaml` with a gateway key, the control key, and the remote control port
disabled on a random port between 20000 and 32000. The script does not start
the service. It installs the latest release; to install a particular one,
append `-s -- --version <version>` to `sudo sh`.

Commands that read the configuration run as the service user, with its data
directory:

```sh
alias twc='sudo -u thinkwatch THINKWATCH_HOME=/var/lib/thinkwatch twcore'

twc remote enable --allow 192.168.1.0/24   # open the remote control port to this network
twc check                                  # validate config.yaml
sudo systemctl enable --now twcore         # start the service, now and at boot
twc control-key                            # the key for ThinkWatch Lite on stdout; address and port on stderr
```

For clients on other machines to use the gateway, set
`listen.gateway.bind: all` in `config.yaml` and list their networks in
`listen.gateway.allow_from`. Upstreams can be written under `providers` or
added later from ThinkWatch Lite. In ThinkWatch Lite,
**Settings → Connection → Add remote connection** takes a name, the server's
address, the control port and the key. The app connects only to a core that
speaks its control-plane protocol version, so the server runs the core version
the app includes, which can be older than the latest release. When the
versions differ, the app shows both, and `twcore upgrade --version` switches
the server to the version the app needs, whether newer or older:

```sh
sudo twcore upgrade --version <version> --restart   # install the version the app needs and restart the service
sudo twcore upgrade --check                         # compare with the latest release; change nothing
sudo twcore upgrade --restart                       # install the latest release and restart the service
```

Neither port uses TLS: the control port is encrypted and authenticated by its
handshake, and the gateway port carries plain HTTP, so both should be reachable
only from trusted networks. [Running core on a server](docs/server.md) covers
the complete procedure, including secrets in `/etc/thinkwatch/env`, network
exposure and uninstalling. Every field of `config.yaml` is described in the
[configuration reference](docs/config.md).

## Crate layers

The workspace holds sixteen crates and the `twcore` binary in `bin/twcore`.
It divides the crates in two: the three that ThinkWatch Enterprise depends
on, and the crates of the gateway that `twcore` runs, which ThinkWatch
Enterprise does not use. Within the second part, the crates are grouped by
role. No crate depends on a group below its own.

| Group | Crates |
|---|---|
| Shared with ThinkWatch Enterprise | `tw-dialect` · `tw-guard` · `tw-breaker` |
| Domain logic | `tw-types` · `tw-engine` · `tw-pricing` · `tw-yaml` · `tw-secret` · `tw-watch` |
| Control-plane contract | `tw-api` · `tw-link` |
| Assembly | `tw-config` · `tw-store` · `tw-observe` |
| Data plane and control plane | `tw-gateway` · `tw-control` |

ThinkWatch Enterprise depends on the first group and nothing else: format
conversion and usage parsing (`tw-dialect`), redaction, tool-call inspection
and the other protections (`tw-guard`), and the circuit-breaker state machine
(`tw-breaker`). These three crates depend only on one another; a test enforces
this, and CI checks that ThinkWatch Enterprise compiles against every change to
them. A component that only one product uses lives in that product's
repository.

ThinkWatch Lite pins `tw-api`, `tw-types`, `tw-yaml`, `tw-guard`, `tw-watch`
and `tw-link` to a release tag and bundles the `twcore` binary from the same
release. Pointing AI clients at the gateway, editing their MCP servers and
scanning their configuration files are done in ThinkWatch Lite: they change
files on the machine the app runs on, which need not be the machine running
`twcore`. `twcore` only issues each client its own gateway key.

The last two groups implement a single `twcore` process: request history in
SQLite, the configuration in one YAML file, and a control plane reached over a
local channel or the remote control port. They are deliberately not shared.
ThinkWatch Enterprise is multi-tenant and keeps its state in PostgreSQL, Redis
and ClickHouse, and one abstraction over both designs would serve neither.

## Control plane

The control plane is an HTTP API carried inside an encrypted channel. It
listens on a unix socket on macOS and Linux and on a loopback port on Windows;
the optional remote control port, `listen.control.remote`, adds a network
listener for ThinkWatch Lite on another machine. Every connection, on every
transport, begins with the same handshake,
`Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s` (implemented in `tw-link`), keyed by
`listen.control.key` in `config.yaml`; no TLS certificates are involved.
`twcore serve` writes the key before it starts listening, into a new
configuration or as one added line in an existing one.

As HTTP runs inside the channel, curl cannot reach the control plane;
`twcore call` can:

```
twcore control-key              # print the key ThinkWatch Lite connects with
twcore control-key --rotate     # replace it; connections made with the old key are closed
twcore call /status
twcore call -X POST -d '{"model":"claude-sonnet-4-5","route":"default"}' /dryrun
```

The configuration text the control plane returns has the key masked, and a
write through the control plane cannot change it.

The remote control port is opened in addition to the local channel, with the
same key and handshake:

```
twcore remote enable --allow 192.168.1.0/24   # the port is picked at random the first time
twcore remote disable
twcore control-key                             # the key on stdout; the address and port on stderr
```

Connections from sources outside `allow_from` are closed without a reply, and
the server's own loopback address is not admitted automatically. A source that
fails the handshake five times within a minute is ignored for a minute, and at
most 32 remote connections are open at a time. A remote connection cannot stop
core, take the diagnostic bundle or change `listen.control`.

## Development

```
cargo test --workspace     # unit and integration tests
scripts/smoke.sh           # run every path on the real binary, from a clean state
```

`scripts/smoke.sh` leaves the user's own configuration untouched: `HOME` and
`THINKWATCH_HOME` both point at a temporary directory that is deleted when the
script finishes. [CONTRIBUTING.md](CONTRIBUTING.md) lists the checks a pull
request has to pass and describes the configuration reference, the price list
and the release process.

## License

[MIT](LICENSE)
