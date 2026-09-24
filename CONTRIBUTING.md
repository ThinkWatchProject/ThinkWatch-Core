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
  Windows loopback port, and the remote port to come) hands its
  connections to the same handshake before HTTP. The control key never
  leaves through the control plane and cannot be changed through it.

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

`twcore` ships inside the desktop app's `.app`, so "which build is in
there" has to be a fact somebody can check rather than whatever sat in
a `target/` directory that afternoon.

1. Bump `version` in the workspace `Cargo.toml`, land it on `main`.
2. Tag that commit `vX.Y.Z` and push the tag.
3. `release.yml` builds `twcore` for `aarch64-apple-darwin`, checks the
   binary actually runs and reports the version on the tag, and attaches
   it to a GitHub Release with a `sha256`.

The desktop app pins `tw-api` to the same tag and bundles the binary
from that release. Those two have to come from one commit: the binary
speaks a protocol, and the app compiles a mirror of it.

Apple Silicon only, deliberately. An Intel user downloading a file that
will not open is worse served than one who finds no download at all;
supporting them means a universal binary, which is its own decision.
