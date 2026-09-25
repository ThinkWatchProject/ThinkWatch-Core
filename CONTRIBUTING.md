# Contributing to ThinkWatch Core

## Open PRs against `main`

```bash
gh pr create --base main --head your-branch
```

`main` is the only long-lived branch. Every PR lands on it, and a
release is a tagged commit on it rather than a separate line, so the
base GitHub pre-fills is the one you want.

There used to be a `dev` branch in between, and a bot that asked you to
retarget onto it. Both are gone. With one branch there is nothing to
keep in sync and no window in which a fix is merged but not yet
releasable; the `dev` branch's last act was to sit still for five days
while thirty-five commits landed on `main` without it.

## What this repository is

A set of crates, not an application. `bin/twcore` is a complete gateway
binary and the thing to run when you want to see behavior:

```bash
cargo run -p twcore -- init     # write a commented config.yaml
cargo run -p twcore -- check    # validate only, don't start
cargo run -p twcore -- serve    # start the gateway and control plane
```

Both editions depend on these crates — the desktop app
([ThinkWatch Lite](https://github.com/ThinkWatchProject/ThinkWatch-Lite))
and the server edition. A change here reaches both, so "it works for my
case" is not the bar.

What is *not* here: adopting AI clients, editing their MCP servers and
scanning their configuration. Those change files on the machine the
desktop app runs on, so they live in the desktop app (its `tw-adopt` and
`tw-scan` crates), and the only control-plane endpoint they use is
`POST /clients/{id}/key`, which issues a client its own gateway key.

## Commit messages

Conventional Commits (`fix(scope): subject`), in **English** — this is a
public repository and the history is documentation.

Say *why* in the body, not just *what*; the diff already shows what
changed. A commit explaining the reasoning behind a non-obvious choice
saves the next person from re-deriving it or "fixing" it back.

## Before you open the PR

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./scripts/smoke.sh
```

Warnings are errors, and relaxing that on CI is the same as removing it.
The toolchain is `stable`, so a newer stable than your local one can
surface lints you cannot reproduce — `rustup update stable` before
blaming CI.

`scripts/smoke.sh` runs the real binary against a real socket and a real
data plane, talking to the control plane through `twcore call` (every
control connection starts with a Noise handshake, so curl cannot). **It catches what unit tests structurally cannot** — file
permissions, socket path limits, an endpoint that simply isn't
registered, a config field silently swallowed. This project's first four
real bugs were all in those seams. Tests that hit the live network are
marked `#[ignore]` and don't run in CI.

## Things that are load-bearing

A PR that breaks one of these will be asked to change, regardless of how
clean the diff is:

- **Never echo a real secret** — not in the UI, a diff, a log, an event,
  a diagnostic bundle, or a test fixture. Masking happens before it
  leaves the process.
- **Never present an estimate as exact.** Cost is three states —
  measured, estimated, and no price at all. Treating the third as 0
  makes a total quietly wrong with nothing to signal it.
- **Observation must never block forwarding**. Storage, pricing,
  and scanning run off bounded channels; a full channel drops the
  observation rather than delaying the request.
- **Report, never auto-delete**. The scanner has no write path,
  and there's a test that reads the product code to prove it.
- **Anything that bypasses the main pipeline re-applies its
  protections.** Replay came close to being a legitimate way around
  redaction.
- **One door into the control plane.** Every transport (unix socket,
  Windows loopback port, and the remote control port) hands its
  connections to the same handshake before HTTP. The control key never
  leaves through the control plane and cannot be changed through it.

## The configuration reference

`docs/config.md` and `docs/config.zh-CN.md` are written by hand, except the
field tables and the built-in rule lists: everything between
`<!-- generated: … -->` and `<!-- /generated -->` is rendered from
`crates/tw-config/tests/manual/schema.rs`, and
`cargo test -p tw-config --test manual` fails when the two differ.

That file declares every section of `config.yaml` against its Rust type, and
the test checks the declaration against the code rather than trusting it:

- **Field names** come from serde itself (a probe deserializer records the
  names a derived `Deserialize` asks for), so a field added to a config
  type and not to the manual fails with the field's name.
- **Defaults are proven.** A declared default is written into a minimal
  section and parsed; it has to mean the same as leaving the field out. A
  field marked required has to fail without it.
- **Enum values** (`protocol`, `mode`, `type` …) are read from serde, not
  copied.
- **Examples** in the manuals are parsed as configuration.

When you change a config type, add or change its row (English and Chinese),
then regenerate:

```bash
UPDATE_CONFIG_DOCS=1 cargo test -p tw-config --test manual
```

A section that is designed but not in the code yet is declared as
`Ty::Pending`: it is rendered, and the test fails as soon as the code starts
reading that field, so the declaration gets switched to the real type.

## The price list

Prices come in two layers.

- **The default price table** is LiteLLM's public dataset. A pinned
  snapshot is embedded in `crates/tw-pricing`, so a fresh install and an
  offline machine have one. Update steps are in
  `crates/tw-pricing/data/PROVENANCE.md`, and a CI test compares the
  snapshot against the hand-checked `data/verified.yaml` row by row. At
  runtime the control plane refreshes the table once a day (unless
  `pricing.auto_update: false`) and saves it as `model_prices.json` beside
  `config.yaml`. Whichever of the two is newer prices requests.
- **Price sheets** live in `config.yaml` under `pricing.sheets`: a
  multiplier over the default table, plus per-model overrides with every
  field written out (dollars per million tokens; nothing is inferred at
  pricing time). An upstream picks a sheet with `pricing:`. Without one,
  the default table applies.

A refresh or an edited sheet changes the price of requests from then on,
never of the ones already recorded. Each request stores its cost and where
the price came from: the default table and its date, a sheet's multiplier,
or a sheet's override. "Yesterday's number doesn't match today's" is then
answered on the request itself.

## Cutting a release

`twcore` ships inside the desktop app, and on its own for servers, so
"which build is in there" has to be a fact somebody can check rather than
whatever sat in a `target/` directory that afternoon.

1. Bump `version` in the workspace `Cargo.toml`, land it on `main`.
2. Tag that commit `vX.Y.Z` and push the tag.
3. `release.yml` builds `twcore` for every target below. It checks that
   each binary is built for its target and, where the runner can execute
   it, that it starts and reports the version on the tag. The Windows
   arm64 binary is cross-compiled on an x64 runner, so only the machine
   field in its PE header is checked. The Linux binaries are also checked
   to need glibc 2.35 at most (Ubuntu 22.04). Once every target has
   built, a single job attaches them all to a GitHub Release, each with a
   `.sha256` (`<sha>  <file>`); if one target fails, nothing is
   published.

| Target | Files |
|---|---|
| `aarch64-apple-darwin` | `twcore-aarch64-apple-darwin` |
| `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc` | `twcore-<target>.exe` |
| `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` | `twcore-<target>`, and `twcore-<target>.tar.gz` holding the binary, `twcore.service` and `LICENSE` |

The bare binaries are what the desktop app's pipeline bundles and what
`twcore upgrade` downloads. The Linux tarballs are what
`scripts/install.sh` installs on a server; they carry the systemd unit so
the unit and the binary come from the same commit. The file names are a
contract with both: `twcore upgrade` has a test that reads `release.yml`.

To try a change to `release.yml` without publishing, run it by hand on
your branch (`gh workflow run release.yml --ref <branch>`): it builds and
checks everything, leaves the files as the run's artifacts, and publishes
nothing.

The desktop app pins `tw-api` (and the few other crates it uses:
`tw-types`, `tw-yaml`, `tw-guard`, `tw-watch`, `tw-link`) to the same tag
and bundles the binary from that release. Those two have to come from one
commit: the binary speaks a protocol, and the app compiles a mirror of it.

On macOS, Apple Silicon only, deliberately. An Intel user downloading a
file that will not open is worse served than one who finds no download
at all; supporting them means a universal binary, which is its own
decision.
