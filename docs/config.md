# Configuration reference

[中文](config.zh-CN.md)

ThinkWatch Core reads one file, `config.yaml`. This page describes every
field in it: what it does, its default, the values it takes, and how a
change reaches the running process. To run core on a server and control it
from the desktop app, see [Running core on a server](server.md).

The field tables on this page are generated from the code, and a test fails
when they disagree, so a field listed here is a field the binary reads.

## Where the file is

| Platform | Default location |
|---|---|
| macOS, Linux | `~/.thinkwatch/config.yaml` |
| Windows | `%APPDATA%\ThinkWatch\config.yaml` |

`THINKWATCH_HOME` replaces the directory, and `--config <path>` names the
file for a single command. The directory holds everything else core keeps
as well: the request database (`data.db`), the configuration history
(`history/`), the downloaded price table (`model_prices.json`) and the local
control socket (`twcore.sock`; on Windows a loopback port recorded in
`control.port`). The directory is private to its owner (`0700`), the file is
`0600`: it holds keys in plain text.

These permissions are the file's only protection on this machine. They keep
out other users, not programs running as the same user: such a program can
read every key in the file, and with the control key it holds, change the
configuration through the control plane. Outbound redaction does not change
that; it protects what a request carries off the machine, not what is on disk.
The control key is masked where the configuration is shown so that a write
through the control plane cannot change it
([`listen.control`](#cfg-listen-control)), not to keep it from local
programs; upstream and gateway keys are shown as written.

Tool-call inspection has a built-in rule for this directory,
`thinkwatch-data`. A tool call whose path or command points into one of the
default locations above, or into `/var/lib/thinkwatch` or `/etc/thinkwatch`
on a server, is recorded, and under `enforce` the response is cut off, so a
model cannot be steered into reading these keys or rewriting its own
protections. Mentioning the path, as in a document being edited, does not
count. With `THINKWATCH_HOME` elsewhere, a custom rule in
[`security.inspect_tools`](#cfg-security-inspect_tools) can cover that path.

`twcore serve` writes a starting configuration when there is none, and
`twcore init` writes one on request. Both produce this:

```yaml
version: 1
listen:
  control:
    key: 6629…753d       # generated
clients:
  - name: default
    key: tw-…            # generated
```

That is a complete, valid configuration. It has no upstream yet, so the
control plane runs and requests are answered with an error saying so.
Adding one upstream makes it forward:

```yaml
providers:
  - name: anthropic
    base_url: https://api.anthropic.com
    key: ${ANTHROPIC_API_KEY}
```

## How the file is read

- **A field name that is not in this reference is an error.** A misspelled
  `prot:` is not ignored while the gateway quietly starts on the default
  port; the whole file is refused and the message names the field.
- **Defaults are not written into the file.** Anything left out has the
  default in the tables below. The app and the command line add a field
  only when its value differs from the default, so what is in the file is
  what someone chose.
- **`${VAR}` reads an environment variable** in fields marked "`${VAR}`
  allowed": upstream keys, header values, proxy passwords. It is read from
  the environment of the core process when a request is sent; an unset
  variable fails that upstream's requests and names the variable. Under
  systemd, that environment is the unit's `EnvironmentFile`.
- **Names are references.** Rules, groups and keys refer to upstreams,
  groups, routes and price sheets by name. A name that points nowhere is an
  error at load time rather than a rule that never matches. Renaming in the
  app changes every reference in the same write. Names starting with `__`
  are reserved for built-ins.
- **`version`** is the format version, `1`. A file with a higher version was
  written by a newer twcore and is refused.

## How a change takes effect

There are three ways to change the configuration, and they all go through
the same path: the desktop app, `twcore config …`, and editing the file
in an editor. Core watches the file and reloads it within a second of a
save; nothing needs to be restarted.

A new version is applied only if it passes every stage:

1. It parses as YAML.
2. It matches the schema: known fields, the right types.
3. It is consistent: names are unique, references resolve, patterns
   compile, CIDR ranges are well formed.
4. The runtime objects can be built from it.

If any stage fails, **the previous configuration stays in service**, and the
error says which stage failed and where. A typo never takes the gateway
down. The desktop app shows the rejection until a valid version is saved.

Changes to `listen.gateway` apply live as well: core opens the new
listener, and if it cannot (the port is taken) it keeps the old one and
reports why. Retention changes are applied on the next hourly clean-up.

When two writers edit at once (the app and a hand edit), the second write
is refused with a version mismatch instead of overwriting the first.

### History and rollback

Every version that was in effect is kept in `history/` beside the file,
with where it came from (the app, the command line, an outside edit, a
rollback, a credential rotation). The last 50 are kept.

```sh
twcore check                      # validate the file without starting anything
twcore config show                # print it, with its version
twcore config history             # list the versions, newest first
twcore config rollback 3f9a2c     # go back to a version (a prefix is enough)
twcore config set /listen/gateway/port 8790 --int
```

These commands work on the file directly, so they work when core is not
running, which is when a rollback is most needed. A running core picks
their changes up like any other save.

`twcore config set <path> <value>` changes one value that is already written
in the file. The path names list items by their `name`
(`/providers/anthropic/base_url`); a number is an index (`/routes/0/rules/1/to`).
The value is a string unless `--int`, `--bool` or `--null` says otherwise.
The result is validated before it is written. To add a field that is not in
the file yet, edit the file.

### Credentials the gateway writes back

One write is not made by a person. When an upstream's OAuth token endpoint
issues a new refresh token, the old one stops working, so the gateway
writes the new token (and the access token with its expiry) back into
`providers[].oauth`. Only those values change; comments and layout are left
as they are.

## Reference

Each table lists every field of one section. "—" in the Default column means
the field is simply absent unless written; the description says what that
means.

### Top level

<!-- generated: table config -->
<a id="cfg-config"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `version` | integer | **required** | Format version of this file. The only version is `1`. A file with a higher number was written by a newer twcore and is refused rather than half-understood. |
| `listen` | object, [`listen`](#cfg-listen) | — | Where the gateway and the control channel listen. |
| `clients` | list of [`clients[]`](#cfg-clients) | `[]` | Gateway keys. At least one is required; `twcore init` and the first `twcore serve` write one named `default`. |
| `providers` | list of [`providers[]`](#cfg-providers) | `[]` | Upstreams. None is a valid configuration: the control plane runs and requests are answered with an error saying no upstream is configured. |
| `proxies` | list of [`proxies[]`](#cfg-proxies) | `[]` | Outbound proxies, declared once and referred to by name from `providers[].proxy`. |
| `pricing` | object, [`pricing`](#cfg-pricing) | — | Refreshing the default price table, and price sheets of your own. |
| `client_probes` | object, [`client_probes`](#cfg-client_probes) | — | What happens to the helper requests clients send on their own (health checks, warm-ups, titles). |
| `security` | object, [`security`](#cfg-security) | — | The three guards. All of them start in `observe`, so out of the box nothing is changed or refused. |
| `retention` | object, [`retention`](#cfg-retention) | — | How long request logs are kept. |
| `failover` | object, [`failover`](#cfg-failover) | — | How long an upstream is set aside after it fails, and how long the start of a stream is awaited. |
| `aliases` | map of alias → string or list of strings | `{}` | Model aliases: one name for the same model across upstreams, mapped to the name each upstream uses, in order. A request for an alias goes to any upstream offering one of the listed names, under the first of them it offers. An alias cannot list another alias. |
| `groups` | list of [`groups[]`](#cfg-groups) | `[]` | Strategy groups: several upstreams behind one name, with a way to pick among them. |
| `routes` | list of [`routes[]`](#cfg-routes) | `[]` | Routes. Without any, requests fail over across all upstreams in the order they are declared. |
| `default_route` | string | — | The route for keys that do not name one. Unset: the route named `default`, or the built-in failover when there is none. |
| `default_key` | string | — | The gateway key for clients that were not given a key of their own. Unset: the key named `default`, or the first key. It cannot be disabled. |
| `plugins` | list of [`plugins[]`](#cfg-plugins) | `[]` | Script plugins, in the order they run. The app installs them; each one's code and settings are a file next to this one. |
<!-- /generated -->

### `listen`

Where core accepts connections. There are two kinds: the AI gateway that
clients send requests to, and the control channel the desktop app and
`twcore` commands use.

<!-- generated: table listen -->
<a id="cfg-listen"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `gateway` | object, [`listen.gateway`](#cfg-listen-gateway) | — | The AI gateway: the address clients send requests to. |
| `control` | object, [`listen.control`](#cfg-listen-control) | — | The control channel: how the desktop app and `twcore` commands reach core. It holds the control key, so every configuration has it. |
<!-- /generated -->

#### `listen.gateway`

<!-- generated: table listen.gateway -->
<a id="cfg-listen-gateway"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `bind` | `loopback` \| `all` \| interface name \| IP address | `loopback` | `loopback` is this machine only; `all` is every interface; an interface name (`en0`, `eth0`) is looked up again every few seconds and follows address changes, and while the interface is not there the gateway listens on 127.0.0.1 only and adds it once it appears; a fixed IP address stops working when the address changes. Binding one interface also listens on 127.0.0.1. |
| `port` | integer | `8788` | TCP port of the gateway. |
| `allow_from` | list of strings | `[10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, fc00::/7]` | Sources other than this machine that may connect, as CIDR ranges or single addresses. This machine is always allowed. `[]` means this machine only; `0.0.0.0/0` allows everyone and has to be written out. |
<!-- /generated -->

When `bind` reaches beyond this machine, `allow_from` decides who gets in.
The gateway has no TLS: expose it on networks you trust, or put it behind a
tunnel or VPN.

```yaml
listen:
  gateway:
    bind: all
    port: 8788
    allow_from: [192.168.1.0/24]
```

#### `listen.control`

The control channel is how the desktop app, and `twcore config` and
`twcore control-key` on the same machine, talk to core. Locally it is a
socket file in the data directory (a loopback port on Windows); no network
port is opened for it unless `remote` is enabled.

Every control connection, local or remote, starts with a handshake that
proves both ends hold `key`
(`Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`: the key is the pre-shared key,
each connection negotiates fresh session keys, and the traffic is
encrypted). There are no certificates. A peer without the key cannot
complete the first message.

<!-- generated: table listen.control -->
<a id="cfg-listen-control"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `key` | string | generated | The control key: 64 hexadecimal characters (32 bytes). Every control connection, local or remote, proves it knows this key. `twcore serve` writes one before listening if it is missing; a configuration where it is malformed is refused. Show it with `twcore control-key`, replace it with `twcore control-key --rotate`. |
| `remote` | object, [`listen.control.remote`](#cfg-listen-control-remote) | — | A network port for the desktop app on another machine. Additional to the local channel, never instead of it. |
<!-- /generated -->

`key` is written by `twcore serve` before the control channel starts
listening, if it is missing, and by `twcore init`. It is masked wherever the
configuration is shown or kept in the history; saving a masked value back
keeps the real one. A missing or malformed key (not 64 hexadecimal
characters) makes the whole file invalid, so it cannot be replaced by a
short, guessable one.

```sh
twcore control-key            # print the key, to paste into the desktop app
twcore control-key --rotate   # replace it; connected apps have to reconnect
```

Both run on the machine where core runs.

#### `listen.control.remote`

A network port for a desktop app on another machine. It is opened in
addition to the local channel, so a mistake here (a port that is taken, an
`allow_from` that shuts you out) never locks out the machine itself:
`twcore config` and the local app keep working.

<!-- generated: table listen.control.remote -->
<a id="cfg-listen-control-remote"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Listen on the remote port. Unset or `false`: no network port is opened for control. `twcore remote enable` and `twcore remote disable` switch it; a running core follows within a second. |
| `bind` | `loopback` \| `all` \| interface name \| IP address | `all` | Interface to listen on, written as for `listen.gateway.bind`. |
| `port` | integer | **required** | TCP port. There is no fixed default: `twcore init` and `twcore remote enable` write a random port between 20000 and 32000 (never the gateway's) when they write this section. It cannot be 0 or the gateway's port. |
| `allow_from` | list of strings | `[10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, fc00::/7]` | Sources that may connect, as for `listen.gateway.allow_from`, except that this machine is not let in automatically (it has the local channel). A connection from anywhere else is closed before the handshake, without a byte in reply; narrowing the list also closes open connections it no longer allows. A source that fails the handshake 5 times within a minute is ignored for a minute. |
<!-- /generated -->

```yaml
listen:
  control:
    key: 9f2c…e41a          # 64 hexadecimal characters
    remote:
      enabled: true
      bind: all
      port: 23483            # random, written when the section is generated
      allow_from: [192.168.1.0/24]
```

A connection over the remote port cannot do three things, whatever the app
asks: shut core down (it is managed by systemd), change `listen.control`
(the door it came in through), or produce a diagnostic bundle (it would be
written on the server). Handshakes time out after 5 seconds; five failed
handshakes from one source within a minute block that source for a minute.

### `clients`

Gateway keys: the keys clients such as Claude Code and Codex send to the
gateway. A key is an identity. Limits, model scope and route are per key.

<!-- generated: table clients[] -->
<a id="cfg-clients"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | string | **required** | Name of the key; unique. Routing rules match it with `when.client`. |
| `key` | string | **required** | The key clients send (as `x-api-key` or `Authorization: Bearer`). Generated keys start with `tw-` so they are not mistaken for an upstream's key. Unique. |
| `max_concurrent` | integer | — | Requests with this key that may run at once; the rest wait. Unset: no limit. `0` is refused. |
| `allow` | list of strings | — | Models this key may use, as model ids or globs (`claude-*`). An upstream model name also allows the aliases that list it; an alias allows only the alias. Unset: every model. `[]`: none at all. |
| `route` | string | — | Name of the route requests with this key take. Unset: `default_route`. |
| `client` | string | — | The client this key was made for (`claude-code`, `codex`, …), recorded when the desktop app points a client at the gateway. A client has at most one. |
| `disabled` | bool | `false` | Refuse every request made with this key, and keep the key. |
| `limits` | list of [`clients[].limits[]`](#cfg-clients-limits) | — | Usage limits: requests, tokens or cost per minute, hour, day, week or month. A request has to pass every one. Unset: no limit. |
<!-- /generated -->

```yaml
clients:
  - name: default
    key: tw-a3f9c8d1e5b2h7k4m6n8p2q4
  - name: build-server
    key: tw-q8r2s4t6u8v2w4x6y8z2a4b6
    max_concurrent: 4
    allow: [claude-sonnet-*]
    route: cheap
```

#### `clients[].limits`

Usage limits for a key: at most so many requests, tokens or dollars per
minute, hour, day, week or month. Each entry counts one of the three; a key
can have several, and a request has to pass every one.

`minute` and `hour` are rolling: the last 60 seconds, the last 60 minutes.
When one is used up, a request waits for the next free slot if it frees
within `failover.slot_wait_secs` (30 seconds by default), and is refused
otherwise. Any later wait for a busy upstream comes out of the same time.
`day`, `week` and `month` follow the calendar in the time zone of the machine
twcore runs on and start again at midnight, on Monday and on the 1st. When one is used up, requests are refused until it starts again. If the
machine's time zone changes, the current day, week and month are added up
again from the request records.

A refused request gets HTTP 429 in the client's own error format, naming the
key, the limit, the amount used and when it resets, and it shows in the
traffic list. A request that never reaches an upstream counts toward no
limit: one refused by a rule, the content filter or a limit, or turned away
because every upstream was at its `max_concurrent`. Cost is what is recorded
for each request, so a model without a price and an upstream with
`billing: free` count as $0. A request still
running counts with an estimate of its input until it is recorded. After a
restart, the day, week and month are added up again from the request records,
so the records have to cover the period: `retention.row_days` of at least 1
for a daily limit, 7 for a weekly one and 31 for a monthly one. Minute and
hour limits start empty.

On a Responses WebSocket connection, each `response.create` is a request of
its own: it is recorded with its usage and cost and checked against these
limits, and a refused one is answered with `response.failed` while the
connection stays open. A Realtime connection (`/v1/realtime`) is one request:
it is checked against the limits when it opens, and the tokens and cost of
all its answers count when it closes.

<!-- generated: table clients[].limits[] -->
<a id="cfg-clients-limits"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `per` | `minute` \| `hour` \| `day` \| `week` \| `month` | **required** | The period. `minute` and `hour` are rolling (the last 60 seconds, the last 60 minutes); `day`, `week` and `month` start again at local midnight, on Monday and on the 1st. |
| `requests` | integer | — | At most this many requests. Token counts, answers the gateway gives itself and requests that never reach an upstream do not count. |
| `tokens` | integer | — | At most this many tokens: uncached input, cache writes and output. |
| `cost` | number | — | At most this much, in US dollars, as recorded for each request; at least 0.01. Models without a price and upstreams with `billing: free` count as 0. |
| `cache_reads` | bool | `false` | Count cache reads too. Only for a `tokens` limit. |
<!-- /generated -->

```yaml
clients:
  - name: build-server
    key: tw-q8r2s4t6u8v2w4x6y8z2a4b6
    limits:
      - { per: minute, requests: 30 }
      - { per: day, cost: 5 }
      - { per: month, tokens: 20000000, cache_reads: true }
```

### `providers`

Upstreams: the APIs requests are forwarded to.

<!-- generated: table providers[] -->
<a id="cfg-providers"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | string | **required** | Name of the upstream; unique, and not the name of a group. Names starting with `__` are reserved. |
| `base_url` | string | **required** | Endpoint, `http://` or `https://`, up to the version segment where the provider documents one (`https://api.anthropic.com`, `https://api.openai.com/v1`). For Bedrock, the region's runtime endpoint: `https://bedrock-runtime.<region>.amazonaws.com`. |
| `key` | string, `${VAR}` allowed | — | API key. It goes in the header the protocol expects: `x-api-key` (Anthropic), `Authorization: Bearer` (OpenAI, and a Bedrock API key), `x-goog-api-key` (Gemini). Leave it out for upstreams without a key, or when the credential is written in `headers`. Cannot be combined with `oauth` or `aws`. |
| `headers` | map of header name → value | `{}` | Additional request headers, in the order written; values may use `${VAR}`, and `{{access_token}}` where `oauth` is set. At most 32. Headers HTTP or the gateway manages (`host`, `content-length`, `connection`, …) cannot be set. |
| `oauth` | object, [`providers[].oauth`](#cfg-providers-oauth) | — | OAuth credential: an access token obtained from a refresh token. Instead of `key`. |
| `aws` | object, [`providers[].aws`](#cfg-providers-aws) | — | AWS access keys of a Bedrock upstream, written there or read from an AWS profile: every request is signed with them (SigV4). Instead of `key`, which holds a Bedrock API key. |
| `protocol` | `anthropic` \| `openai-chat` \| `openai-responses` \| `gemini` \| `chatgpt` \| `bedrock` | — | API format of the upstream. Unset: recognized from `base_url` for the official endpoints (a Bedrock runtime endpoint is `bedrock`), otherwise treated as `anthropic`. |
| `forward_client_identity` | bool | `false` | Also send the client's own identity: its `User-Agent`, identity headers such as `x-app` and `originator`, and identity fields in the request body such as `metadata.user_id`. Values are the client's, never made up. Off: requests carry ThinkWatch's `User-Agent` and no client identity. For upstreams that admit only certain clients (Kimi For Coding, Bailian Coding Plan, relays restricted to official clients). Not available for `chatgpt`. |
| `proxy` | string | `direct` | `direct`; `system`, the proxy in the core process's `HTTPS_PROXY`, `HTTP_PROXY` or `ALL_PROXY` environment variables; or the name of an entry in `proxies`. |
| `on_proxy_fail` | `fail` \| `direct` | `fail` | When the proxy cannot be reached: `fail` the request, or go `direct`. |
| `models` | list of strings | `[]` | Models added by hand, by exact id, for those the upstream serves but leaves out of its list. They count as offered together with the models the upstream lists, or are the whole list when it lists none: they appear in `/v1/models` and requests for them are routed here. `models_only` still applies. No wildcards, no duplicates, at most 256 characters each. |
| `models_only` | list of strings | — | Use only these of the upstream's models, as ids or globs. Others are not listed and are not routed here. Unset: all of them. Empty is refused; use `disabled`. |
| `billing` | `per-token` \| `free` | `per-token` | `per-token`: cost is usage times the price in the upstream's price sheet, subscription accounts included. `free`: cost is recorded as 0. |
| `pricing` | string | — | Name of a price sheet under `pricing.sheets`. Unset: the default price table. |
| `model_specs` | map of model id → [`providers[].model_specs.*`](#cfg-providers-model_specs) | `{}` | Context window, output limit, reasoning and image input of single models of this upstream, written by hand, by exact model id. They take precedence over the price table: for models it does not know, or gets wrong. |
| `max_concurrent` | integer | — | Most requests sent to this upstream at the same time, from 1 to 1000. When it is full, a conversation that stays on it waits for a free slot and other requests go to the next upstream; see `failover.slot_wait_secs`. Unset: no limit. |
| `disabled` | bool | `false` | Take the upstream out of routing and out of the model list, and keep its configuration. |
<!-- /generated -->

A credential is one of four things: `key`, which goes in the header the
protocol expects; `oauth`, a token obtained from a refresh token; `aws`, the
access keys a Bedrock upstream signs its requests with; or `headers`, when the
upstream wants something of its own. `headers` can be combined with the
others, except for the header that already carries the credential.

```yaml
providers:
  - name: anthropic
    base_url: https://api.anthropic.com
    key: ${ANTHROPIC_API_KEY}

  - name: relay
    base_url: https://relay.example.com/v1
    protocol: openai-chat
    headers:
      X-Relay-Token: ${RELAY_TOKEN}
    proxy: office
    models_only: [gpt-4.1*, o3]
    pricing: relay-discount
    max_concurrent: 4

  - name: local
    base_url: http://127.0.0.1:11434/v1
    protocol: openai-chat
    billing: free
```

An upstream's models are the ones it lists on `/v1/models` (for Bedrock, the
region's model list) and the ones written in `models`. Upstreams often leave
models out of their list: a relay lists only some of what it serves, an
account backend hides new models from older clients. A model added in `models`
is listed in `/v1/models`, can be named by an alias, pinned in a rule and
allowed for a key, and is routed to this upstream like a listed one;
`models_only` applies to both. An upstream that lists nothing has exactly the
models in `models`; one with neither lists no model and is still sent requests
for any model.

A request carries the request itself and the headers its upstream needs,
and nothing else from the client: the credential and the headers written in
`headers`; ThinkWatch's own `User-Agent`; and, from the client's request, only
the headers the upstream's protocol uses (`anthropic-*` for Anthropic,
`Idempotency-Key` and `X-Client-Request-Id` for OpenAI, none for Gemini).
Identity fields that clients fill in themselves, such as Claude Code's
`metadata.user_id`, are removed from the body. For an upstream that admits only
certain clients, turn on `forward_client_identity`.

A request converted to Anthropic, or to Claude on Bedrock, marks where the
upstream may cache the prompt when the client marked nothing itself. Clients
in OpenAI or Gemini formats such as Codex cannot mark anything: those
providers cache a repeated prompt on their own, while Anthropic caches only
what is marked. The marks go at the end of the tools, at the end of the
system prompt and at the end of the last two user turns, at most four, each
kept for the default five minutes. The earlier of the two user marks is where
the previous request ended, so each turn reads back what the turn before
wrote and pays the cache price for it instead of the full input price. Cache
writes and reads are charged at the price table's cache prices. A request
that carries its own marks, as Claude Code's do, keeps exactly those, and a
request sent on in the upstream's own format is not changed.

On Bedrock, marks are added only for the Claude models AWS lists as
supporting prompt caching: Claude 3.7 Sonnet, Claude 3.5 Sonnet v2, and every
Claude from version 4.5 on, including newer ones not yet listed. Older models,
such as Claude 3 Haiku, Sonnet 4 and Opus 4.1, get none. An upstream that
refuses the marks is sent the request once more without them, and that
upstream is not sent marks for that model again until the core restarts.

A ChatGPT account upstream (`protocol: chatgpt`) takes only the credential
the desktop app obtains by signing in; it cannot be written by hand. Claude
and Google subscription sign-ins are not supported; use an API key.

Some relays and accounts accept only a few requests at a time and refuse the
rest. `max_concurrent` keeps the gateway within that number: a request takes a
slot on the upstream when it is sent, and gives it back when the answer has
been passed on in full or the client has gone. When the upstream is full, a
conversation that stays on it to reuse its prompt cache waits for a slot;
any other request goes straight to the next upstream. How long a request
waits is `failover.slot_wait_secs`. Waiting is not a failure: the upstream is
not set aside. Requests that only count tokens do not take a slot. On a
Responses WebSocket connection, each `response.create` takes a slot from when
it is sent until its answer ends, and so does a key's `max_concurrent`; an
idle connection takes none.

#### `providers[].oauth`

<!-- generated: table providers[].oauth -->
<a id="cfg-providers-oauth"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `access` | string | — | Current access token. Written back by the gateway after every refresh; unset means one is obtained on first use. |
| `expires_at` | string | — | When `access` expires, RFC 3339 in UTC. Written back with it. Unset: used until the upstream answers 401. |
| `refresh` | string | **required** | Refresh token. When the token endpoint issues a new one, the old one stops working, so the gateway writes the new one back into this file. |
| `endpoint` | string | **required** | Token endpoint URL. |
| `client_id` | string | — | OAuth client id, if the endpoint wants one. |
| `client_secret` | string | — | OAuth client secret, if the endpoint wants one. |
| `refresh_before` | duration (`30s`, `5m`, `1h`) | — | How long before expiry to refresh. Unset or unreadable: `5m`. |
<!-- /generated -->

The access token goes in the protocol's authorization header. To put it
somewhere else, write the header in `headers` with `{{access_token}}` where
the token goes:

```yaml
    oauth:
      refresh: ${VENDOR_REFRESH_TOKEN}
      endpoint: https://auth.example.com/oauth/token
      client_id: my-client
    headers:
      X-Access: Token {{access_token}}
```

#### `providers[].aws`

<!-- generated: table providers[].aws -->
<a id="cfg-providers-aws"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `access_key_id` | string, `${VAR}` allowed | — | Access key ID. Written together with `secret_access_key`; `profile` instead. |
| `secret_access_key` | string, `${VAR}` allowed | — | Secret access key. |
| `session_token` | string, `${VAR}` allowed | — | Session token of temporary credentials, such as those STS issues. When it expires, requests are refused until it is replaced. |
| `profile` | string | — | Profile in the AWS credential files to read the access keys from, instead of writing them here: `~/.aws/credentials` and `~/.aws/config`, or the files `AWS_SHARED_CREDENTIALS_FILE` and `AWS_CONFIG_FILE` name, on the machine core runs on. The files are read again when they change. |
| `region` | string | — | Region to sign for. Unset: the one in `base_url`, which must then be a standard runtime endpoint. Required when `base_url` is a VPC endpoint or a proxy. |
<!-- /generated -->

A Bedrock upstream authenticates in one of two ways. A Bedrock API key goes in
`key` and is sent as `Authorization: Bearer`. AWS access keys go in `aws`,
written there or read with `aws.profile` from a profile in the AWS credential
files: every request is signed with them (SigV4) once its body is final, and the
keys themselves are never sent. Keys written in the configuration can be read
from the environment with `${VAR}`. A profile is read from the files on the
machine core runs on, and read again when they change, so a tool that refreshes
temporary keys in `~/.aws/credentials` needs no restart. Nothing runs a command
to obtain a credential, so a profile that signs in through IAM Identity Center
(`aws sso login`), runs a `credential_process` or assumes a role cannot be used;
export the keys it produces instead.

```yaml
providers:
  - name: bedrock
    base_url: https://bedrock-runtime.us-east-1.amazonaws.com
    key: ${AWS_BEARER_TOKEN_BEDROCK}

  - name: bedrock-keys
    base_url: https://bedrock-runtime.eu-west-1.amazonaws.com
    aws:
      access_key_id: ${AWS_ACCESS_KEY_ID}
      secret_access_key: ${AWS_SECRET_ACCESS_KEY}
      session_token: ${AWS_SESSION_TOKEN}

  - name: bedrock-profile
    base_url: https://bedrock-runtime.us-west-2.amazonaws.com
    aws:
      profile: dev
```

Requests are converted to Converse. The model list comes from the region's
control plane: the foundation models that can be invoked on demand, the
inference profiles AWS defines (`us.anthropic.claude-…`), and the account's
application inference profiles, listed by the ARN they are invoked by. Listing
needs `bedrock:ListFoundationModels` and `bedrock:ListInferenceProfiles`;
without them requests are still forwarded, and `models` can list the models by
hand. For a VPC endpoint or a proxy, write its address in `base_url` and the
region in `aws.region`; the model list is asked of that address too.

#### `providers[].model_specs`

A model's context window and output limit come from the price table. A relay's
own models are often missing from it, and now and then it is wrong. Write the
numbers here, for this upstream and by the exact id in its model list. A value
written here takes precedence over the price table; one left out still comes
from it. At least one of the two is written, and neither can be 0.

The same numbers are used everywhere: in `/v1/models` for every client format,
for an alias this upstream serves, when the gateway judges whether a
conversation still fits the model it is on, and as the output limit of a
request converted to Anthropic that does not set one. When several upstreams
offer the same model, `/v1/models` describes it by the first of them in
`providers`.

<!-- generated: table providers[].model_specs.* -->
<a id="cfg-providers-model_specs"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `context_window` | integer | — | Context window: the most tokens a request can take in. Unset: the price table's. |
| `max_output_tokens` | integer | — | The most tokens an answer can have. Unset: the price table's. |
| `reasoning` | bool | — | Whether the model reasons. The model list (`GET /v1/models`) carries it, so clients offer reasoning levels for it. Unset: the price table's; the list leaves it out when the price table does not say. |
| `image_input` | bool | — | Whether the model takes images as input. The model list carries it, as `input_modalities`. Unset: the price table's; the list leaves it out when the price table does not say. |
<!-- /generated -->

```yaml
providers:
  - name: relay
    base_url: https://relay.example.com/v1
    protocol: openai-chat
    model_specs:
      glm-5-air: { context_window: 128000, max_output_tokens: 16384 }
      claude-sonnet-4-5: { context_window: 1000000 }
```

### `proxies`

Outbound proxies. Different upstreams often need different ones, so there
is no global switch: an upstream picks one with `proxy`.

<!-- generated: table proxies[] -->
<a id="cfg-proxies"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | string | **required** | Name used in `providers[].proxy`. `direct` and `system` are built in. |
| `type` | `socks5` \| `socks5h` \| `http` \| `https` | `socks5h` | `socks5h` sends the host name to the proxy to resolve; `socks5` resolves it locally first. `http` and `https` are HTTP proxies. |
| `addr` | string | **required** | `host:port` of the proxy. |
| `auth` | object, [`proxies[].auth`](#cfg-proxies-auth) | — | User name and password, if the proxy wants them. |
<!-- /generated -->

#### `proxies[].auth`

<!-- generated: table proxies[].auth -->
<a id="cfg-proxies-auth"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `user` | string | **required** | User name. |
| `pass` | string, `${VAR}` allowed | **required** | Password. |
<!-- /generated -->

```yaml
proxies:
  - name: office
    type: http
    addr: proxy.example.com:3128
    auth:
      user: alice
      pass: ${PROXY_PASSWORD}
```

`on_proxy_fail: fail` is the default because falling back silently sends a
request by a path you did not intend; you would believe you were on the
proxy while you were not.

### `pricing`

The cost of a request is its usage times the price of the model. Prices
come from the default price table (LiteLLM's public dataset, a copy of
which is built into the binary and refreshed daily) or from a price sheet
an upstream picks. A change of price applies to requests from then on,
never to ones already recorded.

<!-- generated: table pricing -->
<a id="cfg-pricing"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `auto_update` | bool | `true` | Refresh the default price table from the network once a day. It is saved as `model_prices.json` beside `config.yaml`; the table built into the binary is used until then and when offline. |
| `sheets` | list of [`pricing.sheets[]`](#cfg-pricing-sheets) | `[]` | Price sheets of your own. An upstream uses one with `providers[].pricing`. |
<!-- /generated -->

#### `pricing.sheets`

<!-- generated: table pricing.sheets[] -->
<a id="cfg-pricing-sheets"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | string | **required** | Name of the sheet; unique. |
| `multiplier` | number | `1` | Applied to every price of the default table, cache and long-context prices included. |
| `models` | map of model id → [`pricing.sheets[].models.*`](#cfg-pricing-sheets-models) | `{}` | Prices for single models. They replace the default table's price for that model and are not multiplied. |
<!-- /generated -->

#### `pricing.sheets[].models`

Prices are in US dollars per million tokens, as printed on vendors' price
pages. Every field is written out; nothing is inferred when pricing.

<!-- generated: table pricing.sheets[].models.* -->
<a id="cfg-pricing-sheets-models"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `input` | number | **required** | US dollars per million input tokens. |
| `output` | number | **required** | US dollars per million output tokens. |
| `cache_read` | number | **required** | US dollars per million tokens read from the prompt cache. |
| `cache_write_5m` | number | **required** | US dollars per million tokens written to a 5-minute cache. |
| `cache_write_1h` | number | **required** | US dollars per million tokens written to a 1-hour cache. |
| `input_above_200k` | number | — | Input price once a request's input, cache reads and writes included, exceeds 200K tokens. Written together with `output_above_200k`, or neither. Cache prices stay the ones above. |
| `output_above_200k` | number | — | Output price once a request's input, cache reads and writes included, exceeds 200K tokens. |
<!-- /generated -->

```yaml
pricing:
  sheets:
    - name: relay-discount
      multiplier: 0.8
      models:
        claude-sonnet-4-5-thinking:
          input: 3
          output: 15
          cache_read: 0.3
          cache_write_5m: 3.75
          cache_write_1h: 6
```

### `client_probes`

Some requests clients send are not the user's: connectivity checks,
warm-ups, session titles, topic detection, suggestions. Each class is either
answered locally (`intercept`, nothing is sent upstream) or forwarded
(`forward`). Forwarded ones go through the routing rules like any other
request and carry their class, so a rule can catch them with `when.intent`.
The defaults intercept only what nobody would miss.

<!-- generated: table client_probes -->
<a id="cfg-client_probes"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `health_check` | `intercept` \| `forward` | `intercept` | Connectivity checks (`max_tokens: 1`). Answered locally by default: nothing is lost. |
| `warmup` | `intercept` \| `forward` | `intercept` | Warm-up requests. Answered locally by default. |
| `titling` | `intercept` \| `forward` | `forward` | Requests that name a session. Forwarded by default: intercepting them gives every session the same title. |
| `topic_detect` | `intercept` \| `forward` | `forward` | Topic detection. Forwarded by default. |
| `suggestion` | `intercept` \| `forward` | `forward` | Suggestions. Forwarded by default. |
<!-- /generated -->

### `security`

Three guards, applied to every upstream alike. Each has a `mode`: `off`,
`observe` (detect and record, change nothing) or `enforce` (act). All three
start in `observe`. What `enforce` does differs per guard: redaction replaces,
tool-call inspection cuts the response off, and the content filter does what
each rule says (refuse, delete or record).

<!-- generated: table security -->
<a id="cfg-security"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `redact` | object, [`security.redact`](#cfg-security-redact) | — | Outbound redaction: credentials and personal information anywhere in a request are replaced before it leaves. |
| `inspect_tools` | object, [`security.inspect_tools`](#cfg-security-inspect_tools) | — | Tool-call inspection: dangerous commands in the tool calls a model returns cut the response off. |
| `content` | object, [`security.content`](#cfg-security-content) | — | Content filter: words, patterns or characters (hidden ones among them) in what the caller sends; each rule refuses the request, deletes what it matched, or only records it. |
<!-- /generated -->

#### `security.redact`

Before a request leaves, the whole request (system prompt, earlier answers
and tool calls included) is searched for credentials and personal
information. Under `enforce` what is found is replaced with placeholders, and
put back where the answer repeats them. Images, files and other base64
payloads are not searched.

<!-- generated: table security.redact -->
<a id="cfg-security-redact"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `mode` | `off` \| `observe` \| `enforce` | `observe` | `off` does nothing; `observe` detects and records only, and changes nothing; `enforce` detects and acts. |
| `enable` | list of strings | `[]` | Built-in rules to switch on that are off out of the box, by id. |
| `disable` | list of strings | `[]` | Built-in rules to switch off, by id. |
| `custom` | list of [`security.redact.custom[]`](#cfg-security-redact-custom) | `[]` | Rules of your own: whatever a pattern matches is replaced like a credential. |
<!-- /generated -->

<!-- generated: table security.redact.custom[] -->
<a id="cfg-security-redact-custom"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | string | **required** | Name shown in logs and in the app; it identifies the rule and has to be unique within this guard. |
| `pattern` | string | **required** | Regular expression. |
| `label` | string | `SECRET` | Placeholder name: what the pattern matches is replaced with `<<TW_label_1>>`, numbered per name. Capital letters, digits and underscores, starting with a letter, at most 24 characters. |
| `disabled` | bool | `false` | Switches the rule off and keeps it in the file. |
<!-- /generated -->

Built-in rules:

<!-- generated: rules redact -->
| id | Name | Out of the box |
|---|---|---|
| `anthropic-api-key` | Anthropic API key | on |
| `openai-project-key` | OpenAI project key | on |
| `openai-api-key` | OpenAI API key | on |
| `github-personal-token` | GitHub personal access token | on |
| `github-oauth-token` | GitHub OAuth token | on |
| `github-server-token` | GitHub server token | on |
| `github-user-token` | GitHub user token | on |
| `github-fine-grained-token` | GitHub fine-grained token | on |
| `slack-bot-token` | Slack bot token | on |
| `slack-user-token` | Slack user token | on |
| `slack-app-token` | Slack app token | on |
| `aws-access-key-id` | AWS access key ID | on |
| `aws-temporary-key-id` | AWS temporary access key ID | on |
| `google-api-key` | Google API key | on |
| `google-oauth-token` | Google OAuth token | on |
| `gitlab-token` | GitLab token | on |
| `stripe-live-key` | Stripe live key | on |
| `stripe-restricted-key` | Stripe restricted key | on |
| `npm-token` | npm token | on |
| `digitalocean-token` | DigitalOcean token | on |
| `sendgrid-key` | SendGrid key | on |
| `private-key` | Private key | on |
| `jwt` | JWT | on |
| `conn-string-password` | Connection string password | on |
| `cn-resident-id` | Chinese resident ID number | on |
| `bank-card` | Bank card number | on |
| `email` | Email address | off |
| `cn-mobile-phone` | Chinese mainland mobile number | off |
| `internal-ip` | Internal IP address | off |
| `internal-domain` | Internal domain | off |
<!-- /generated -->

`cn-resident-id`, `bank-card`, `email` and `cn-mobile-phone` look for
personal information rather than credentials. The first two are on out of the
box and match only what checks out by structure:

- `cn-resident-id`: an 18-character resident ID number of the People's
  Republic of China whose first two digits are a province-level code, whose
  date of birth is a real date between 1900-01-01 and today, and whose last
  character is the right check character (ISO 7064 MOD 11-2). The old
  15-digit numbers are not matched.
- `bank-card`: a card number whose prefix and length belong to UnionPay,
  Visa, Mastercard, American Express, JCB, Discover or Diners Club and which
  passes the Luhn check, written as one run of digits or in groups of four
  separated by single spaces or single hyphens (American Express also 4-6-5,
  Diners Club also 4-6-4). The test card numbers published by Stripe,
  Braintree and Adyen are not matched.

`email` and `cn-mobile-phone` are off out of the box: they have no structure
to check, and code and documents are full of things that look like them.

- `email`: an address whose domain has at least two parts, the last of them
  two or more letters. User names in URLs (`https://user@host`) and file
  names such as `icon@2x.png` are not matched.
- `cn-mobile-phone`: a Chinese mainland mobile number, 11 digits starting
  with `1` and a second digit from `3` to `9`, not part of a longer run of
  digits.

A number that is part of a longer run of letters or digits is not matched,
and neither is one written as a JSON number in the request body (in a tool
call's arguments, for instance), since replacing it would leave the body
invalid JSON. The placeholders of these rules say what was there
(`<<TW_ID_NUMBER_1>>`, `<<TW_CARD_NUMBER_1>>`, `<<TW_EMAIL_1>>`,
`<<TW_PHONE_1>>`), and the security log shows only the last four characters
of a number, and the first character and the domain of an email address. A
custom rule replaces with `<<TW_SECRET_1>>` unless it names its own
placeholder (`label`).

#### `security.inspect_tools`

Tool calls a model returns are checked against the rules. Under `enforce`,
a match with rules set to `cut` stops the response, so the client never
receives a complete call to run.

<!-- generated: table security.inspect_tools -->
<a id="cfg-security-inspect_tools"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `mode` | `off` \| `observe` \| `enforce` | `observe` | `off` does nothing; `observe` detects and records only, and changes nothing; `enforce` detects and acts. |
| `enable` | list of strings | `[]` | Built-in rules to switch on that are off out of the box, by id. |
| `disable` | list of strings | `[]` | Built-in rules to switch off, by id. |
| `actions` | map of built-in rule id → `cut` \| `record` | `{}` | What a built-in rule does under `enforce`, written only where it differs from the factory setting (`rm-rf-root: record`). |
| `custom` | list of [`security.inspect_tools.custom[]`](#cfg-security-inspect_tools-custom) | `[]` | Rules of your own, matched against the arguments of a tool call. |
<!-- /generated -->

<!-- generated: table security.inspect_tools.custom[] -->
<a id="cfg-security-inspect_tools-custom"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | string | **required** | Name shown in logs and in the app; it identifies the rule and has to be unique within this guard. |
| `pattern` | string | **required** | Regular expression. |
| `action` | `cut` \| `record` | `record` | Under `enforce`: `cut` the response off, or only `record` the match. |
| `disabled` | bool | `false` | Switches the rule off and keeps it in the file. |
<!-- /generated -->

Built-in rules:

<!-- generated: rules inspect_tools -->
| id | Name | Under `enforce`, out of the box |
|---|---|---|
| `curl-pipe-sh` | Download and run | `cut` |
| `base64-decode-exec` | Decode and run | `cut` |
| `exfil-env` | Send out environment variables | `cut` |
| `exfil-credentials` | Send out a credential file | `cut` |
| `exfil-credentials-reversed` | Send out a credential file (verb first) | `cut` |
| `ssh-key-read` | Read a private key or cloud credential | `cut` |
| `secret-to-unknown-host` | Send a credential to an unknown host | `cut` |
| `thinkwatch-data` | Read or change ThinkWatch's own data | `cut` |
| `write-startup-item` | Write a startup item | `cut` |
| `crontab-install` | Install a scheduled job | `cut` |
| `rm-rf-root` | Delete home or root | `record` |
| `chmod-777` | World-writable permissions | `record` |
| `upload-file-to-host` | Upload a local file to an external host | `record` |
<!-- /generated -->

#### `security.content`

Words, patterns or characters in what the caller sends: user messages and
the tool results in them, not the system prompt or the model's own turns.
Each rule matches a keyword (`contains`), a regular expression (`regex`) or
code points (`codepoints`), and says what happens under `enforce`: `block`
refuses the request, `strip` deletes every match from the caller's text and
sends the rest, `record` only records it. After deleting, the text is checked
again, so a keyword split by hidden characters is caught once they are gone.

<!-- generated: table security.content -->
<a id="cfg-security-content"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `mode` | `off` \| `observe` \| `enforce` | `observe` | `off` does nothing; `observe` detects and records only, and changes nothing; `enforce` detects and acts. |
| `enable` | list of strings | `[]` | Built-in rules to switch on that are off out of the box, by id. |
| `disable` | list of strings | `[]` | Built-in rules to switch off, by id. |
| `actions` | map of built-in rule id → `block` \| `strip` \| `record` | `{}` | What a built-in rule does under `enforce`, written only where it differs from the factory setting. |
| `custom` | list of [`security.content.custom[]`](#cfg-security-content-custom) | `[]` | Rules of your own. |
<!-- /generated -->

<!-- generated: table security.content.custom[] -->
<a id="cfg-security-content-custom"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | string | **required** | Name shown in logs and in the app; it identifies the rule and has to be unique within this guard. |
| `pattern` | string | **required** | A keyword; a regular expression with `match: regex`; code points with `match: codepoints` (`U+200B, U+E0000–U+E007F`). Keywords and regular expressions are case-insensitive. |
| `match` | `contains` \| `regex` \| `codepoints` | `contains` | `contains`: the text contains `pattern`. `regex`: `pattern` is a regular expression. `codepoints`: the text has a character among the code points or ranges listed in `pattern`, separated by commas. |
| `action` | `block` \| `strip` \| `record` | `record` | Under `enforce`: `block` the request, `strip` what matched and send the rest, or only `record` the match. |
| `disabled` | bool | `false` | Switches the rule off and keeps it in the file. |
<!-- /generated -->

Built-in rules:

<!-- generated: rules content -->
| id | Name | Group | Out of the box | Under `enforce`, out of the box |
|---|---|---|---|---|
| `unicode-tags` | Unicode tag characters | invisible | on | `strip` |
| `bidi-controls` | Bidirectional controls | invisible | on | `strip` |
| `zero-width` | Zero-width characters | invisible | off | `strip` |
| `private-use` | Private-use characters | invisible | off | `strip` |
| `ignore-previous-instructions` | Ignore previous instructions | injection | on | `block` |
| `ignore-all-previous` | Ignore all previous | injection | on | `block` |
| `disregard-your-instructions` | Disregard your instructions | injection | on | `block` |
| `jailbreak` | Jailbreak | injection | off | `block` |
| `dan` | DAN | injection | off | `block` |
| `developer-mode` | Developer mode | injection | off | `block` |
| `you-are-now` | Persona manipulation | persona | off | `block` |
| `new-persona` | New persona | persona | off | `record` |
| `act-as` | Act as | persona | off | `record` |
| `pretend-to-be` | Pretend to be | persona | off | `record` |
| `system-prompt` | System prompt extraction | persona | off | `record` |
| `reveal-your-instructions` | Reveal instructions | persona | off | `record` |
| `what-are-your-rules` | What are your rules | persona | off | `record` |
| `base64-wall` | Base64 smuggling | persona | off | `record` |
| `zh-ignore-previous` | Ignore previous instructions (Chinese) | chinese | off | `block` |
| `zh-forget-your` | Forget your instructions (Chinese) | chinese | off | `block` |
| `zh-do-not-follow` | Do not follow (Chinese) | chinese | off | `block` |
| `zh-you-are-now` | You are now (Chinese) | chinese | off | `block` |
| `zh-role-play` | Role-play (Chinese) | chinese | off | `record` |
| `zh-reveal-your` | Reveal your instructions (Chinese) | chinese | off | `record` |
| `zh-system-prompt` | System prompt (Chinese) | chinese | off | `record` |
| `zh-jailbreak` | Jailbreak (Chinese) | chinese | off | `block` |
<!-- /generated -->

The `invisible` group matches characters people cannot see and models can
read. Unicode tag characters (U+E0000–U+E007F) and bidirectional controls
(U+202A–U+202E, U+2066–U+2069) are on out of the box; zero-width characters
(U+200B–U+200D, U+2060, U+FEFF) and private-use characters (U+E000–U+F8FF,
U+F0000–U+FFFFD, U+100000–U+10FFFD) are off, since emoji, Persian and icon
fonts use them too. All four delete what they match under `enforce`.

```yaml
security:
  redact:
    mode: enforce
    enable: [internal-ip]
    custom:
      - name: employee-id
        pattern: 'EMP-\d{6}'
        label: EMPLOYEE
  inspect_tools:
    mode: enforce
  content:
    mode: enforce
    enable: [zero-width]
    custom:
      - name: project-x
        pattern: project-x
        action: strip
```

### `retention`

Two limits, because the two kinds of data differ in size by three orders
of magnitude: request bodies are tens of kilobytes each, a request's record
a few hundred bytes. The byte limit covers bursts.

Each request and response body is kept up to 4 MiB; of a longer one, the
beginning is kept. Bodies are written with credentials and personal numbers
already taken out. A request stored under `enforce` carries the placeholders
the upstream received; anything else the redaction rules
([`security.redact`](#cfg-security-redact)) recognize is masked, in every
mode, `off` included.

<!-- generated: table retention -->
<a id="cfg-retention"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `body_days` | integer | `7` | Days to keep request and response bodies. |
| `row_days` | integer | `90` | Days to keep the record of each request (time, model, usage, cost). |
| `body_max_bytes` | integer | `5368709120` | Upper bound on the bytes bodies may take; beyond it the oldest days go first. The default is 5 GiB. |
<!-- /generated -->

### `failover`

An upstream that fails is set aside for a while, so that the next requests
go straight to the next candidate. How long depends on the reason the
upstream gives: an insufficient balance waits for a top-up, a used-up quota
waits until the moment the upstream says it resets, and a rate limit usually
passes within seconds. A request with a single candidate is never affected.
On a Responses WebSocket connection, each `response.create` counts here like
a request: one that fails because of the upstream before any content arrives
counts as a failure, and so does a connection the upstream refuses or that
cannot be made.

Before the first content of a streamed answer reaches the client, an error
the upstream sends in the stream moves the request to the next candidate,
the same as an error status would.

An upstream can also go quiet: it accepts the request and then sends nothing,
or stops partway through. After `idle_timeout_secs` (300 by default) without
content, the gateway stops waiting for it. The time counts from the moment the
request is sent to that upstream, so waiting for a free slot does not count,
and starts again with every piece of content: text, reasoning and tool calls
count; keep-alives (SSE comments, Anthropic's `ping`, empty chunks, Responses'
`response.in_progress`) do not, so an upstream that only keeps the connection
alive still runs out of time. A whole, non-streamed answer counts from sending
to the complete answer.

- When no content has reached the client yet, the upstream counts as failed
  (towards `failures_to_pause`, like a 5xx), the attempt appears with the
  outcome `idle_timeout`, and the request moves to the next candidate. Until
  then an upstream's answer is held back from the client, so the next upstream
  starts it afresh. With no candidate left, the client gets a timeout error
  (504) in its own format. The last candidate's stream is passed on as it
  arrives, so once its response has started, a timeout there ends it with an
  error event instead.
- A streamed answer is held for at most 15 seconds. If no content has
  arrived by then, the client receives `200` and the streaming headers, so
  that its own wait for headers does not run out, followed by an SSE comment
  (`: keep-alive`) every 15 seconds; Gemini clients get no comments. The
  upstream's events stay held until its first content, and failover continues
  as before under the same `200`: the next upstream's stream starts cleanly,
  and when no candidate is left, the stream ends with an error event in the
  client's format instead of a 504. The comments do not count as content.
- When content has already reached the client, the request cannot move on
  without repeating it: the answer ends with an error event in the client's
  format, and the request is recorded as failed.

The conversation then no longer stays on that upstream for the rest of its turn.
Models that think for a long time before they write anything need a longer
timeout.

```yaml
failover:
  idle_timeout_secs: 600
```

When upstreams are at their `max_concurrent`, a request waits for a free slot
for at most `slot_wait_secs` in all. The same time also covers waiting for a
key's `minute` or `hour` limit, so a request never waits longer than
`slot_wait_secs` for the two together. An ongoing conversation waits for the
upstream it stays on and, if no slot frees in time, moves on to the next one,
where its cache starts over. A new conversation skips a full upstream at once.
When every candidate is full, the request waits for whichever frees first; if
none does, the client gets a 429 with `Retry-After` saying the upstreams are
busy. If an upstream did receive the request and failed, the client gets that
failure instead.

<!-- generated: table failover -->
<a id="cfg-failover"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `failures_to_pause` | integer | `3` | Consecutive failures without a stated reason (5xx, connection errors) before the upstream is set aside. From 1 to 100. |
| `pause_secs` | integer | `60` | Seconds the first such pause lasts. Each further pause doubles it, up to `max_pause_secs`; one success resets it. |
| `max_pause_secs` | integer | `600` | Upper bound on the doubled pause, in seconds; not less than `pause_secs`. |
| `no_balance_pause_secs` | integer | `1800` | Seconds to set aside an upstream that reports an insufficient balance. |
| `quota_pause_secs` | integer | `3600` | Seconds to set aside an upstream whose quota is used up when it does not say when the quota resets. When it does, the upstream is set aside until then. |
| `rate_limit_max_pause_secs` | integer | `3600` | A rate-limited upstream is set aside for the time its `Retry-After` gives, at most this many seconds. Without `Retry-After` it counts as a failure without a stated reason. |
| `idle_timeout_secs` | integer | `300` | Seconds an upstream may go without sending content before the gateway stops waiting for it. Counted from the moment the request is sent and started again by every piece of content: text, reasoning and tool calls count, keep-alives do not. A whole (non-streamed) answer counts from sending to the complete answer. Before any content has reached the client, the upstream counts as failed and the request moves to the next one; with none left, the client gets a timeout error. After content has reached the client, the answer ends with an error. From 30 to 3600. |
| `slot_wait_secs` | integer | `30` | Seconds a request waits in all, counted once the key's own `max_concurrent` lets it in: for a key's `minute` or `hour` limit to free up, and for a free slot on upstreams at their `max_concurrent`. A key limit that does not free up in time refuses the request; without an upstream slot in time it goes to the next upstream, or, when every candidate is full, is answered with 429. `0`: never wait. From 0 to 300. |
<!-- /generated -->

### `aliases`

A model alias is another name for the same model. Clients request it like any
model; each upstream receives the name it uses for that model.

```yaml
aliases:
  deepseek-v4.1: DeepSeek-v4.1-flash
  claude-sonnet-5:
    - claude-sonnet-5
    - us.anthropic.claude-sonnet-5-v1:0
    - anthropic/claude-sonnet-5
```

- An upstream serves an alias when it offers one of the listed names within
  its `models_only`, and receives the first such name in the list. Every
  upstream that serves an alias takes part in failover for it.
- `GET /v1/models` lists aliases next to the upstream models, and the
  original names stay available.
- An alias takes precedence over an upstream model of the same name: a
  request for `claude-sonnet-5` above goes only to upstreams offering one of
  the three names. List an upstream's own name when it should keep serving it.
- A key's `allow` and a rule's `when.model` written for an upstream model name
  also cover the aliases that list it; written for an alias, they cover only
  the alias.
- When the upstream answers with the model it was sent, the answer carries the
  name the client asked for. An answer naming a different model is passed on
  as it is.
- Prices and the upstream check-up use the name sent upstream. The request log
  keeps the name the client asked for, and each attempt the name it sent.
- To send one key's requests to a particular upstream model, use a rule with
  pinned models ([`routes[].rules[].to`](#cfg-routes-rules-to)) rather than an
  alias: an alias changes what the name means for every client.

### `groups`

A group puts several upstreams behind one name. Rules send requests to a
group with `to`.

<!-- generated: table groups[] -->
<a id="cfg-groups"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | string | **required** | Name of the group; unique, and not the name of an upstream. |
| `type` | `fallback` \| `select` \| `load-balance` \| `url-test` \| `cheapest` | `fallback` | `fallback`: the first healthy member, in order. `select`: the member named in `selected`. `load-balance`: requests are shared out in proportion to the members' weights; a conversation in progress stays where it is. `url-test`: the fastest by measured time from sending a request to the first content of the answer. `cheapest`: the lowest input price. |
| `providers` | list of strings or [`groups[].providers[]`](#cfg-groups-providers) | **required** | Member upstreams, by name; not groups. Each upstream appears once in a group. In a `load-balance` group, a member can be written as `{name, weight}`. |
| `selected` | string | — | For `select`: the chosen member. |
| `balance_by` | `weights` \| `latency` \| `health` \| `latency-health` | `weights` | For `load-balance`: what the members' weights are multiplied by. `weights`: nothing; the weights alone. `latency`: faster upstreams get more. `health`: upstreams that fail less get more. `latency-health`: both. Other group types take only `weights`. |
<!-- /generated -->

`fallback` is the default because a single user's machine has no load to
spread.

In a `load-balance` group, a member can carry a weight, from 1 to 100; a
member written as just its name has weight 1. Weights set how the group's
requests are shared out: with `{ name: anthropic, weight: 7 }` and `relay`,
the official API serves seven requests in ten. Conversations in progress
stay on the upstream that answers them (see below) and count toward its
share, so the balance is kept by where new conversations start. An upstream
that is cooling down after failures, is at its `max_concurrent`, or cannot
serve a request, sits that request out, and the others share it by their
weights. A new WebSocket connection is placed the same way and counts as one
request; everything sent on it then goes to the upstream it connected to.
Other group types take no weights.

<!-- generated: table groups[].providers[] -->
<a id="cfg-groups-providers"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | string | **required** | The upstream, by name. A member written as just its name has weight 1. |
| `weight` | integer | `1` | The member's share of a `load-balance` group's requests, in proportion to the other members' weights. From 1 to 100. Other group types take no weight other than 1. |
<!-- /generated -->

```yaml
groups:
  - name: pool
    type: load-balance
    providers:
      - { name: anthropic, weight: 7 }
      - relay
```

Whatever the type, a conversation stays on the upstream that last answered
it, so that what the upstream holds of it in its prompt cache is read again
rather than paid for in full elsewhere. Within a turn (while the client sends
tool results back) it always stays; across turns it stays while the previous
answer read or wrote at least 1024 tokens of prompt cache and came less than
five minutes ago. An upstream that is cooling down after failures releases
the conversation, and whichever upstream answered after a failover is the one
it stays on. The rule a turn matched at its start also holds for the rest of
that turn: rules keyed on input size or images do not move a turn halfway,
unless its input no longer fits the context window of a model the rule sends
it to. A `load-balance` weight is therefore the long-run share of requests:
conversations in progress stay where they are and count toward that
upstream's share.

`balance_by` lets a `load-balance` group also look at how each upstream has
been doing lately. Each member's weight is multiplied by a factor, and the
group shares out requests by the result in the same way as above.

- `weights` (the default): the weights alone.
- `latency`: faster upstreams get a larger share. Speed is the typical time
  from sending a request to the first content of the answer, the same
  measurement `url-test` uses. An upstream twice as fast as the middle of the
  group has its weight multiplied by four, by at most ten and at least a
  tenth.
- `health`: upstreams that fail less get a larger share. It looks at the
  last 50 requests within the past 30 minutes. Server errors, rate limits,
  used-up quota or balance, rejected credentials, timeouts (including
  `failover.idle_timeout_secs` before any content) and connection errors
  count as failures; errors caused by the request itself do not, and neither
  does a client that cancels, a request aborted by hand, or an upstream
  skipped because it is at its `max_concurrent`. An upstream that keeps failing keeps a twentieth of its
  weight, so it still gets the occasional request and its recovery
  is noticed; one that fails outright is set aside by
  [`failover`](#cfg-failover) as before.
- `latency-health`: both factors, multiplied.

Speed is measured on streamed answers only, from the moment the request is
sent to that upstream, so waiting and upstreams that failed before it do not
count. An upstream given up on because it sent no content within
`failover.idle_timeout_secs` counts as taking the whole wait. On a
Responses WebSocket connection, each `response.create` counts as one request
for both speed and failures, its speed measured from the moment the upstream
starts answering it.

An upstream without enough measurements yet counts as average. As with
weights alone, conversations in progress stay where they are, and new
conversations make up the difference.

```yaml
groups:
  - name: pool
    type: load-balance
    balance_by: latency-health
    providers:
      - { name: official, weight: 3 }
      - relay
```

### `routes`

A route is a list of rules evaluated top to bottom. Each key takes the route
named in its `route`, otherwise `default_route`, otherwise the route named
`default`; without any, requests fail over across all upstreams in the
order they are declared.

<!-- generated: table routes[] -->
<a id="cfg-routes"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | string | **required** | Name of the route; unique. `default` is the one keys use unless told otherwise. |
| `rules` | list of [`routes[].rules[]`](#cfg-routes-rules) | `[]` | Evaluated top to bottom; the first rule with `to` or `deny` that matches decides where the request goes. |
<!-- /generated -->

#### `routes[].rules`

<!-- generated: table routes[].rules[] -->
<a id="cfg-routes-rules"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | string | **required** | Name shown in logs and in the traffic view. |
| `when` | object, [`routes[].rules[].when`](#cfg-routes-rules-when) | — | Conditions, all of which have to hold. Unset: matches every request. |
| `to` | string, or list of [`routes[].rules[].to[]`](#cfg-routes-rules-to) | — | An upstream or a group, by name; `__all__` is every upstream in declared order. Or pinned models: a list of upstreams with the model to send to each, tried in order. Not allowed together with `when.provider_would_be`. |
| `set` | object, [`routes[].rules[].set`](#cfg-routes-rules-set) | — | Parameters to rewrite. Collected from every matching rule, not only the first. |
| `deny` | string | — | Refuse the request with this reason. |
<!-- /generated -->

#### `routes[].rules[].to`

`to` names an upstream or a group, or pins models: a list of upstreams, each
with the model sent to it, tried in order. A pinned model is sent as written,
without aliases or `set.model`, so a rule can send a key's requests to one
upstream's model even when an alias of the same name points elsewhere.

```yaml
routes:
  - name: default
    rules:
      - name: Opus on Bedrock
        when: { model: claude-opus-5 }
        to:
          - { provider: bedrock, model: us.anthropic.claude-opus-5-v1:0 }
          - { provider: anthropic, model: claude-opus-5 }
```

<!-- generated: table routes[].rules[].to[] -->
<a id="cfg-routes-rules-to"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `provider` | string | **required** | An upstream, by name; not a group. Each upstream appears once in the list. |
| `model` | string | **required** | The model name sent to that upstream, as written: aliases do not apply, and no `set.model` changes it. Not allowed together with `set.model` in the same rule. |
<!-- /generated -->

#### `routes[].rules[].when`

<!-- generated: table routes[].rules[].when -->
<a id="cfg-routes-rules-when"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `model` | string | — | Requested model, glob (`claude-opus-*`). An upstream model name also matches requests for the aliases that list it; an alias matches only requests for the alias. |
| `client` | string | — | Name of the gateway key the request used, exactly. |
| `dialect` | string | — | API format the client spoke: `anthropic`, `openai-chat`, `openai-responses`, `gemini`. |
| `input_tokens` | comparison (`>200k`, `<=4k`, `==3`) | — | Estimated input tokens. |
| `max_tokens` | comparison (`>200k`, `<=4k`, `==3`) | — | The request's `max_tokens`. A request without one never matches. |
| `tool_count` | comparison (`>200k`, `<=4k`, `==3`) | — | Number of tools offered. |
| `cache` | bool | — | Whether the request uses the prompt cache. |
| `tools` | bool | — | Whether the request offers tools. |
| `image` | bool | — | Whether the request contains an image. |
| `thinking` | bool | — | Whether extended thinking is on. |
| `stream` | bool | — | Whether the response is streamed. |
| `intent` | string or list of strings | — | A client helper request: `assistant_internal` for any of them, or one class (`titling`). Classes set to `intercept` in `client_probes` are answered locally and never reach routing. |
| `provider_would_be` | string or list of strings | — | The upstream routing chose. Such a rule is evaluated after routing, may only `set` or `deny`, and cannot have `to`. |
<!-- /generated -->

A comparison starts with `>`, `>=`, `<`, `<=` or `==`, and the number may
end in `k` or `m`: `">200k"`, `"<=4k"`. Without an operator it is an error,
not an equality: `"200k"` alone is refused.

#### `routes[].rules[].set`

`set.model` may name an alias; each upstream then receives its own name for
it. Answers carry the name the client asked for, as with aliases.

<!-- generated: table routes[].rules[].set -->
<a id="cfg-routes-rules-set"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `model` | string | — | Send a different model. The prompt cache of the session is lost. |
| `max_tokens` | integer | — | Replace `max_tokens`. |
| `thinking` | bool | — | Turn extended thinking on or off. |
| `only_at_session_start` | bool | `false` | Apply only when a session starts. Recorded and shown; not in effect yet. |
<!-- /generated -->

```yaml
groups:
  - name: fast
    type: url-test
    providers: [anthropic, relay]

routes:
  - name: default
    rules:
      - name: long context goes to the official API
        when: { input_tokens: ">200k" }
        to: anthropic
      - name: titles go to the cheap model
        when: { intent: titling }
        set: { model: claude-haiku-4-5 }
      - name: everything else
        to: fast
default_route: default
```

### `plugins`

Script plugins change requests before they reach an upstream and answers
before they reach the client. They run in a sandbox inside core, without
access to files, the network or the real values of secrets. The app installs
them: each plugin's code goes to `plugins/<id>.js` next to this file, a copy
of the approved code to `plugins/.approved/<id>.js`, and the code's SHA-256
to `sha256`.

A plugin's file holds its settings too. The `manifest` at the top of the file
says what to do when the plugin fails (`on_error`), which requests it handles
(`match`) and the value of each setting (`settings.<name>.value`). When the
app changes one of these, it rewrites only the manifest in the file and
updates `sha256` along with it. This list keeps just the plugin, its approved
hash and whether it is on. Entries written by core 0.58 also have `on_error`,
`scope` and `settings`: they are ignored, and removed the next time the app
changes a plugin.

A plugin runs only while its file has exactly the approved hash. When the
file changes on disk or disappears, the plugin stops within seconds and the
app shows the change for review. Until the change is approved, the requests
the plugin covers are refused (`on_error: "reject"`, the default) or pass
without it (`on_error: "skip"`), as the approved file says. A plugin that does
not load is handled the same way. Neither keeps the rest of the configuration
from taking effect.

Plugins run in the order of this list.

In `match`, `clients`, `models` and `upstreams` are lists of names or
patterns with `*` anywhere in them, matched regardless of case; a list that
is empty or left out matches everything. `clients` names the client app
(`claude-code`, `codex`, …), and a request whose app is not recognised
matches only an empty list. `models` matches the model sent to the upstream:
when a routing rule renames the model, the new name is the one that matches.
`upstreams` applies to requests and answers alike.

A plugin changes a request after routing, each time the request is sent to an
upstream. A request that fails over to another upstream starts again from what
the client sent, and the plugin sees which upstream and which model name the
request goes to. Routing, model checks and session grouping use what the
client sent. A plugin that changes the model name only renames what is sent to
that upstream: the request is not routed again, and the new name must still be
one of the models the key may use ([`clients[].allow`](#cfg-clients)), or the
request is not sent.

A plugin handles the kinds of request its code declares: conversations
(Anthropic Messages, OpenAI Chat Completions and Responses, and Gemini,
including their token counts and compaction), embeddings (`/v1/embeddings`,
Gemini `:embedContent` and `:batchEmbedContents`) and legacy completions
(`/v1/completions`). A plugin that declares none handles conversations only.
Requests of a kind a plugin does not handle pass without it, whatever its
`on_error`. Other endpoints, such as images and audio, pass without any plugin.

<!-- generated: table plugins[] -->
<a id="cfg-plugins"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `id` | string | **required** | Lowercase letters, digits and hyphens, 1 to 40 characters; unique. |
| `file` | string | **required** | The plugin's code, relative to this file's directory. It is always `plugins/<id>.js`; the app writes it. |
| `sha256` | string | **required** | SHA-256 of the approved code, 64 lowercase hexadecimal characters. When the file no longer has this hash, the plugin stops running until the change is approved in the app. The approved code is kept in `plugins/.approved/<id>.js`. |
| `enabled` | bool | `true` | Run the plugin. `false` keeps it installed and out of every request. |
<!-- /generated -->

```yaml
plugins:
  - id: add-date
    file: plugins/add-date.js
    sha256: 9f2b6c0e4a1d8f3b7c5e2a9d6f1b4c8e3a7d0f5b2c9e6a1d4f8b3c7e0a5d2f9b
    enabled: true
```

The start of `plugins/add-date.js`:

```js
export const manifest = {
  name: "Add date",
  api: 1,
  permissions: ["system"],
  match: { clients: ["claude-code"], models: ["claude-*"], upstreams: [] },
  on_error: "reject",
  settings: {
    note: { type: "string", label: "Note", value: "Answer in English." },
  },
};
```

## Environment variables

| Variable | Effect |
|---|---|
| `THINKWATCH_HOME` | Data directory, instead of `~/.thinkwatch` (`%APPDATA%\ThinkWatch` on Windows). |
| `TWCORE_LOG` | Log filter, in `tracing` syntax (`info`, `debug`, `tw_gateway=debug`). |
| `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY` | Used by upstreams with `proxy: system`. |
| Any other | Read where the configuration writes `${NAME}`. |
