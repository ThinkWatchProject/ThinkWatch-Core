# Codex reasoning profiles

`codex_reasoning.json` supplies effort choices when an upstream lists ordinary
model IDs without native Codex metadata. It deliberately contains only model
identity, the minimum Codex version, the default effort and supported levels.
It does not copy OpenAI prompts, tools or other capabilities onto a provider.
An upstream native definition takes precedence, including an empty effort list;
an explicit `model_specs.reasoning: false` still disables reasoning choices.

OpenAI entries are extracted from `codex-rs/models-manager/models.json` at
[`e58bf8f5233b51e7cd1679d947e3028c1ed91b06`](https://github.com/openai/codex/blob/e58bf8f5233b51e7cd1679d947e3028c1ed91b06/codex-rs/models-manager/models.json).
Preserve each entry's minimum client version, default and complete effort list
when refreshing this snapshot. The source is MIT licensed.

DeepSeek entries follow the official [thinking-mode guide](https://api-docs.deepseek.com/guides/thinking_mode)
and [model details](https://api-docs.deepseek.com/quick_start/pricing), checked
on 2026-11-12: `low`, `high`, `max`, with `high` as the default. The Flash ID,
its documented legacy names and the V4.1-Flash-1017 model version share these
choices, as do V4-Pro and its documented V4-Pro-0813 version. The generic Codex
profile is verified with CLI 0.166.0, so these profiles require that version.

Profiles are selected by the actual upstream model ID, ignoring a provider
namespace and ASCII case. Alias names never determine capabilities. Unknown
models receive no invented effort choices, even if a price table or manual
specification says they can reason. Add another model only with an authoritative
source for its actual levels and default.
