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
fields. A standard OpenAI `data` list uses a generic coding profile; reported model
capabilities are projected onto it without adopting native instructions or tools. Core does not copy bundled OpenAI model
instructions onto unrelated providers.

Only models already in the shared gateway directory are exposed. A native
catalog does not independently add models or grant inference access. Alias
names become the exposed slugs, with no migration target. If routing candidates
have different native definitions, Core uses the generic profile and intersects
their known capabilities. Explicit model specifications override native values;
native values take precedence over the price table. Unknown context windows
are omitted. Image input requires affirmative support from every candidate.

For ordinary upstream model lists, Core reads `data[].context_window`,
`data[].input_modalities`, `data[].effort.supported_levels`, and
`data[].effort.default_level`. For example, DeepSeek documents these fields in
its [models endpoint](https://api-docs.deepseek.com/api/list-models). The effort
list becomes Codex's `supported_reasoning_levels`; the reported default is kept
when it is in the list. Optional or malformed fields do not create capabilities.
No model-name table or inference from `reasoning: true` supplies missing levels.
A relay must preserve the metadata for Core to use it.

Aliases inherit the actual routing targets' reported capabilities. Effort
choices are intersected separately from full native definitions, so differences
in prompts or tools do not discard shared levels. Defaults must belong to the
intersection. Manual `model_specs.reasoning: false` removes choices. When any
candidate requires protocol translation, choices are also limited to efforts
Core can translate; Responses and ChatGPT passthrough can retain newer native
efforts such as `ultra`.

## Cache and limits

Only upstream metadata is cached. Entries are isolated by the provider,
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
