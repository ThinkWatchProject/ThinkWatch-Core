# Codex remote model discovery

[中文](codex-model-catalog.zh-CN.md)

Core serves a native Codex catalog at
`GET /codex/models?client_version=<major.minor.patch>`. It uses the same gateway
keys, allowed source addresses, model directory, aliases and key `allow` rules
as inference. The standard `/v1/models` endpoint keeps its OpenAI response shape.

ThinkWatch Lite configures this connection when adopting Codex. For a manual
connection, use a provider like this in Codex's `config.toml`:

```toml
model_provider = "thinkwatch"

[features]
api_key_model_discovery = true

[model_providers.thinkwatch]
name = "ThinkWatch"
base_url = "http://127.0.0.1:8788/v1"
model_catalog_url = "http://127.0.0.1:8788/codex/models"
wire_api = "responses"
env_key = "THINKWATCH_GATEWAY_KEY"
requires_openai_auth = false
supports_websockets = false
```

Use your gateway's actual address and set `THINKWATCH_GATEWAY_KEY` to an enabled
gateway key. Remove any effective `model_catalog_json` setting: that local file
takes precedence over the remote catalog. A selected profile can also override
the provider, local catalog or discovery feature. Lite handles the selected
default profile and restores its original fields when disconnected. Explicitly
selecting a different profile or passing command-line overrides can still change
which provider Codex uses.

The connection is configured once. After that, changes to upstream models,
aliases, enabled ranges, manual model capabilities or key permissions are
projected on the next catalog request. Changes made through Lite or the control
API use the normal hot reload path; no model list is written to the Codex config.
The initial connection requires restarting Codex. Codex also maintains its own
model cache, so an already open picker may show its previous list until the
client refreshes it or restarts. This endpoint does not push updates to clients.

## Native metadata and ordinary upstreams

For Responses, Chat Completions and ChatGPT providers, Core requests metadata
with the requesting Codex `client_version`, using that provider's credentials
and configured proxy. The inbound gateway key is never forwarded. ChatGPT uses
its native models URL. Direct OpenAI API providers use OpenAI's documented native
Codex endpoint; other providers use their models endpoint. A full native
`models` entry retains its instructions, tools and unknown version-specific
fields. A standard OpenAI `data` list contains no native Codex instructions and
uses a generic coding profile instead. Core does not copy bundled OpenAI model
instructions onto unrelated providers.

Only models already in the shared gateway directory are exposed. A native
catalog does not independently add models or grant inference access. Alias
names become the exposed slugs, with no migration target. If routing candidates
have different native definitions, Core uses the generic profile and intersects
their known capabilities. Explicit model specifications override native values;
native values take precedence over the price table. Unknown context windows
are omitted. Image input requires affirmative support from every candidate.

Reasoning choices come from upstream native metadata when available. For
ordinary model lists or unavailable native metadata, Core also includes
[documented reasoning profiles](../crates/tw-gateway/data/codex_reasoning.md)
for known OpenAI and DeepSeek models. DeepSeek Flash/V4.1-Flash and V4-Pro
offer `low`, `high` and `max`, with `high` as the default. These profiles supply
only effort choices; the generic instructions and tools remain unchanged.
Profiles respect their minimum Codex version. An unknown model does not gain
invented levels merely because its model specification says `reasoning: true`.

Aliases inherit choices from their actual upstream model IDs, including names
with a provider namespace or different ASCII case. A name that merely resembles
a known model does not confer that model's capabilities. Across routing
candidates, Core intersects effort choices independently of instructions and
tools, and chooses a default from the resulting list. A manual
`model_specs.reasoning: false` or a candidate with no known choices removes
effort choices for that alias. An upstream's explicit native list, including an
empty one, takes precedence over a bundled profile.
When a candidate requires protocol translation, choices are also limited to
efforts Core can represent in that translation. Responses and ChatGPT
passthrough can retain newer native efforts such as `ultra`.

## Cache and limits

Only upstream native metadata is cached. Entries are isolated by the provider,
resolved credentials and headers, URL, client version and proxy configuration.
There are at most 32 cache entries and four concurrent metadata fetches. Fetches
have an eight-second deadline. Successful upstream cache lifetimes are capped at
five minutes, subtract `Age`, and honor `no-cache` and `no-store`; failed fetches
use a generic fallback and are retried after 30 seconds. Oversized or unavailable
upstream metadata also falls back to the generic profile.

Gateway names, permissions and capabilities are rebuilt even on conditional
requests. Authentication happens before an ETag can yield `304`. A configuration
reload during a metadata fetch discards that fetch's native definitions before
projecting the current configuration. Responses are private, revalidate after
30 seconds and carry an ETag. Inference continues enforcing the current key and
model permissions independently of anything cached by Codex.

The final native response is limited to 1 MiB, as required by Codex. A catalog
larger than that returns `413` instead of silently omitting models. Restrict the
models available to the gateway key to reduce its size. Missing, duplicate or
invalid `client_version` parameters return `400`.

The remote catalog and generic profile have been checked with Codex CLI
0.166.0. See OpenAI's [catalog endpoint contract](https://developers.openai.com/codex/enterprise/roll-out-a-gateway#implement-the-catalog-endpoint)
for the client configuration and versioned schema requirements.
