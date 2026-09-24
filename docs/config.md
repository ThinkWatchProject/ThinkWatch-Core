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
| `security` | object, [`security`](#cfg-security) | — | The five guards. All of them start in `observe` or `off`, so out of the box nothing is changed or blocked. |
| `retention` | object, [`retention`](#cfg-retention) | — | How long request logs are kept. |
| `groups` | list of [`groups[]`](#cfg-groups) | `[]` | Strategy groups: several upstreams behind one name, with a way to pick among them. |
| `routes` | list of [`routes[]`](#cfg-routes) | `[]` | Routes. Without any, requests fail over across all upstreams in the order they are declared. |
| `default_route` | string | — | The route for keys that do not name one. Unset: the route named `default`, or the built-in failover when there is none. |
| `default_key` | string | — | The gateway key for clients that were not given a key of their own. Unset: the key named `default`, or the first key. It cannot be disabled. |
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
| `bind` | `loopback` \| `all` \| interface name \| IP address | `loopback` | `loopback` is this machine only; `all` is every interface; an interface name (`en0`, `eth0`) is looked up at start and follows address changes; a fixed IP address stops working when the address changes. Binding one interface also listens on 127.0.0.1. |
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
| `enabled` | bool | `false` | Listen on the remote port. Unset or `false`: no network port is opened for control. |
| `bind` | `loopback` \| `all` \| interface name \| IP address | `all` | Interface to listen on, written as for `listen.gateway.bind`. |
| `port` | integer | generated | TCP port. There is no fixed default: a random free port is written when the section is generated, like the key. |
| `allow_from` | list of strings | `[10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, fc00::/7]` | Sources that may connect, as for `listen.gateway.allow_from`. A connection from anywhere else is closed before the handshake, without a byte in reply. |
<!-- /generated -->

```yaml
listen:
  control:
    key: 9f2c…e41a          # 64 hexadecimal characters
    remote:
      enabled: true
      bind: all
      port: 41327            # random, written when the section is generated
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
| `allow` | list of strings | — | Models this key may use, as model ids or globs (`claude-*`). Unset: every model. `[]`: none at all. |
| `route` | string | — | Name of the route requests with this key take. Unset: `default_route`. |
| `client` | string | — | The client this key was made for (`claude-code`, `codex`, …), recorded when the desktop app points a client at the gateway. A client has at most one. |
| `disabled` | bool | `false` | Refuse every request made with this key, and keep the key. |
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

### `providers`

Upstreams: the APIs requests are forwarded to.

<!-- generated: table providers[] -->
<a id="cfg-providers"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | string | **required** | Name of the upstream; unique, and not the name of a group. Names starting with `__` are reserved. |
| `base_url` | string | **required** | Endpoint, `http://` or `https://`, up to the version segment where the provider documents one (`https://api.anthropic.com`, `https://api.openai.com/v1`). |
| `key` | string, `${VAR}` allowed | — | API key. It goes in the header the protocol expects: `x-api-key` (Anthropic), `Authorization: Bearer` (OpenAI), `x-goog-api-key` (Gemini). Leave it out for upstreams without a key, or when the credential is written in `headers`. Cannot be combined with `oauth`. |
| `headers` | map of header name → value | `{}` | Additional request headers, in the order written; values may use `${VAR}`, and `{{access_token}}` where `oauth` is set. At most 32. Headers HTTP or the gateway manages (`host`, `content-length`, `connection`, …) cannot be set. |
| `oauth` | object, [`providers[].oauth`](#cfg-providers-oauth) | — | OAuth credential: an access token obtained from a refresh token. Instead of `key`. |
| `protocol` | `anthropic` \| `openai-chat` \| `openai-responses` \| `gemini` \| `chatgpt` | — | API format of the upstream. Unset: recognized from `base_url` for the official endpoints, otherwise treated as `anthropic`. |
| `proxy` | string | `direct` | `direct`; `system`, the proxy in the core process's `HTTPS_PROXY`, `HTTP_PROXY` or `ALL_PROXY` environment variables; or the name of an entry in `proxies`. |
| `on_proxy_fail` | `fail` \| `direct` | `fail` | When the proxy cannot be reached: `fail` the request, or go `direct`. |
| `models` | list of strings | `[]` | Models to assume when the upstream does not answer `/v1/models`. |
| `models_only` | list of strings | — | Use only these of the upstream's models, as ids or globs. Others are not listed and are not routed here. Unset: all of them. Empty is refused; use `disabled`. |
| `billing` | `per-token` \| `free` | `per-token` | `per-token`: cost is usage times the price in the upstream's price sheet, subscription accounts included. `free`: cost is recorded as 0. |
| `pricing` | string | — | Name of a price sheet under `pricing.sheets`. Unset: the default price table. |
| `disabled` | bool | `false` | Take the upstream out of routing and out of the model list, and keep its configuration. |
<!-- /generated -->

A credential is one of three things: `key`, which goes in the header the
protocol expects; `oauth`, a token obtained from a refresh token; or
`headers`, when the upstream wants something of its own. `headers` can be
combined with the other two, except for the header that already carries the
credential.

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

  - name: local
    base_url: http://127.0.0.1:11434/v1
    protocol: openai-chat
    billing: free
```

A ChatGPT account upstream (`protocol: chatgpt`) takes only the credential
the desktop app obtains by signing in; it cannot be written by hand. Claude
and Google subscription sign-ins are not supported; use an API key.

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
| `input_above_200k` | number | — | Input price once a request's input exceeds 200K tokens. Written together with `output_above_200k`, or neither. |
| `output_above_200k` | number | — | Output price once a request's input exceeds 200K tokens. |
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
warm-ups, session titles, topic detection, suggestions. Each class can be
answered locally (`intercept`, nothing is sent upstream), passed through
(`passthrough`), or handed to the routing rules (`route`, matched with
`when.intent`). The defaults intercept only what nobody would miss.

<!-- generated: table client_probes -->
<a id="cfg-client_probes"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `health_check` | `intercept` \| `passthrough` \| `route` | `intercept` | Connectivity checks (`max_tokens: 1`). Answered locally by default: nothing is lost. |
| `warmup` | `intercept` \| `passthrough` \| `route` | `intercept` | Warm-up requests. Answered locally by default. |
| `titling` | `intercept` \| `passthrough` \| `route` | `passthrough` | Requests that name a session. Passed through by default: intercepting them gives every session the same title. |
| `topic_detect` | `intercept` \| `passthrough` \| `route` | `passthrough` | Topic detection. Passed through by default. |
| `suggestion` | `intercept` \| `passthrough` \| `route` | `passthrough` | Suggestions. Passed through by default. |
<!-- /generated -->

### `security`

Five guards, applied to every upstream alike. Each has a `mode`: `off`,
`observe` (detect and record, change nothing) or `enforce` (act). They start
in `observe`, except the output limit, which starts `off`. What `enforce`
does differs per guard, and each says so below.

<!-- generated: table security -->
<a id="cfg-security"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `redact` | object, [`security.redact`](#cfg-security-redact) | — | Outbound redaction: credentials found in a request are replaced before it leaves. |
| `inspect_tools` | object, [`security.inspect_tools`](#cfg-security-inspect_tools) | — | Tool-call inspection: dangerous commands in the tool calls a model returns cut the response off. |
| `hidden_text` | object, [`security.hidden_text`](#cfg-security-hidden_text) | — | Hidden characters that people cannot see and models can read refuse the request. |
| `content` | object, [`security.content`](#cfg-security-content) | — | Content filter: words or patterns in what the caller sends refuse the request. |
| `output_limit` | object, [`security.output_limit`](#cfg-security-output_limit) | — | Output length: a response longer than the limit is cut off. |
<!-- /generated -->

#### `security.redact`

Before a request leaves, credentials in it are looked for. Under `enforce`
they are replaced.

<!-- generated: table security.redact -->
<a id="cfg-security-redact"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `mode` | `off` \| `observe` \| `enforce` | `observe` | `off` does nothing; `observe` detects and records only, and changes nothing; `enforce` detects and acts. |
| `enable` | list of strings | `[]` | Built-in rules to switch on that are off out of the box, by id. |
| `disable` | list of strings | `[]` | Built-in rules to switch off, by id. |
| `custom` | list of [`security.redact.custom[]`](#cfg-security-redact-custom) | `[]` | Rules of your own: whatever a pattern matches is treated as a credential. |
<!-- /generated -->

<!-- generated: table security.redact.custom[] -->
<a id="cfg-security-redact-custom"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | string | **required** | Name shown in logs and in the app; it identifies the rule and has to be unique within this guard. |
| `pattern` | string | **required** | Regular expression. |
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
| `internal-ip` | Internal IP address | off |
| `internal-domain` | Internal domain | off |
<!-- /generated -->

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
| `write-startup-item` | Write a startup item | `cut` |
| `crontab-install` | Install a scheduled job | `cut` |
| `rm-rf-root` | Delete home or root | `record` |
| `chmod-777` | World-writable permissions | `record` |
<!-- /generated -->

#### `security.hidden_text`

Characters people cannot see and models can read, in what the caller sends
(tool results included). Under `enforce`, the request is refused.

<!-- generated: table security.hidden_text -->
<a id="cfg-security-hidden_text"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `mode` | `off` \| `observe` \| `enforce` | `observe` | `off` does nothing; `observe` detects and records only, and changes nothing; `enforce` detects and acts. |
| `disable` | list of strings | `[]` | Kinds not to look for: `tag`, `bidi`. |
<!-- /generated -->

<!-- generated: rules hidden_text -->
| Kind | What it is |
|---|---|
| `tag` | Unicode tag characters (U+E0000 to U+E007F): invisible everywhere, read by the model, able to carry a whole instruction. |
| `bidi` | Bidirectional control characters: make the order shown differ from the order the model reads. |
<!-- /generated -->

#### `security.content`

Words or patterns in what the caller sends. Under `enforce`, a match with
rules set to `block` refuses the request.

<!-- generated: table security.content -->
<a id="cfg-security-content"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `mode` | `off` \| `observe` \| `enforce` | `observe` | `off` does nothing; `observe` detects and records only, and changes nothing; `enforce` detects and acts. |
| `enable` | list of strings | `[]` | Built-in rules to switch on that are off out of the box, by id. |
| `disable` | list of strings | `[]` | Built-in rules to switch off, by id. |
| `actions` | map of built-in rule id → `block` \| `record` | `{}` | What a built-in rule does under `enforce`, written only where it differs from the factory setting. |
| `custom` | list of [`security.content.custom[]`](#cfg-security-content-custom) | `[]` | Rules of your own. |
<!-- /generated -->

<!-- generated: table security.content.custom[] -->
<a id="cfg-security-content-custom"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | string | **required** | Name shown in logs and in the app; it identifies the rule and has to be unique within this guard. |
| `pattern` | string | **required** | A keyword, or a regular expression with `match: regex`. Case-insensitive either way. |
| `match` | `contains` \| `regex` | `contains` | `contains`: the text contains `pattern`. `regex`: `pattern` is a regular expression. |
| `action` | `block` \| `record` | `record` | Under `enforce`: `block` the request, or only `record` the match. |
| `disabled` | bool | `false` | Switches the rule off and keeps it in the file. |
<!-- /generated -->

Built-in rules:

<!-- generated: rules content -->
| id | Name | Group | Out of the box | Under `enforce`, out of the box |
|---|---|---|---|---|
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

#### `security.output_limit`

<!-- generated: table security.output_limit -->
<a id="cfg-security-output_limit"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `mode` | `off` \| `observe` \| `enforce` | `off` | Off out of the box: no single limit suits every use. `observe` records long responses; `enforce` stops the stream at the limit. |
| `max_chars` | integer | `100000` | Limit in characters (Unicode scalar values), from 1 to 1000000. |
<!-- /generated -->

```yaml
security:
  redact:
    mode: enforce
    enable: [internal-ip]
    custom:
      - name: employee-id
        pattern: 'EMP-\d{6}'
  inspect_tools:
    mode: enforce
  output_limit:
    mode: enforce
    max_chars: 200000
```

### `retention`

Two limits, because the two kinds of data differ in size by three orders
of magnitude: request bodies are tens of kilobytes each, a request's record
a few hundred bytes. The byte limit covers bursts.

<!-- generated: table retention -->
<a id="cfg-retention"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `body_days` | integer | `7` | Days to keep request and response bodies. |
| `row_days` | integer | `90` | Days to keep the record of each request (time, model, usage, cost). |
| `body_max_bytes` | integer | `2147483648` | Upper bound on the bytes bodies may take; beyond it the oldest days go first. The default is 2 GiB. |
<!-- /generated -->

### `groups`

A group puts several upstreams behind one name. Rules send requests to a
group with `to`.

<!-- generated: table groups[] -->
<a id="cfg-groups"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | string | **required** | Name of the group; unique, and not the name of an upstream. |
| `type` | `fallback` \| `select` \| `load-balance` \| `url-test` \| `cheapest` | `fallback` | `fallback`: the first healthy member, in order. `select`: the member named in `selected`. `load-balance`: take turns. `url-test`: the fastest by measured time to first byte. `cheapest`: the lowest input price. |
| `providers` | list of strings | **required** | Member upstreams, by name. |
| `session_affinity` | bool | `true` | Keep a session on the same upstream so its prompt cache keeps hitting. Turning it off under `load-balance` spreads every turn and loses the cache. |
| `selected` | string | — | For `select`: the chosen member. |
<!-- /generated -->

`fallback` is the default because spreading a session across upstreams
loses the prompt cache, which is worth far more than any spread of load on
a single user's machine.

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
| `to` | string | — | An upstream or a group, by name; `__all__` is every upstream in declared order. Not allowed together with `when.provider_would_be`. |
| `set` | object, [`routes[].rules[].set`](#cfg-routes-rules-set) | — | Parameters to rewrite. Collected from every matching rule, not only the first. |
| `deny` | string | — | Refuse the request with this reason. |
<!-- /generated -->

#### `routes[].rules[].when`

<!-- generated: table routes[].rules[].when -->
<a id="cfg-routes-rules-when"></a>

| Field | Type | Default | Description |
|---|---|---|---|
| `model` | string | — | Requested model, glob (`claude-opus-*`). |
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
| `intent` | string or list of strings | — | A client helper request: `assistant_internal` for any of them, or one class (`titling`). Only classes set to `route` in `client_probes` reach routing. |
| `provider_would_be` | string or list of strings | — | The upstream routing chose. Such a rule is evaluated after routing, may only `set` or `deny`, and cannot have `to`. |
<!-- /generated -->

A comparison starts with `>`, `>=`, `<`, `<=` or `==`, and the number may
end in `k` or `m`: `">200k"`, `"<=4k"`. Without an operator it is an error,
not an equality: `"200k"` alone is refused.

#### `routes[].rules[].set`

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

## Environment variables

| Variable | Effect |
|---|---|
| `THINKWATCH_HOME` | Data directory, instead of `~/.thinkwatch` (`%APPDATA%\ThinkWatch` on Windows). |
| `TWCORE_LOG` | Log filter, in `tracing` syntax (`info`, `debug`, `tw_gateway=debug`). |
| `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY` | Used by upstreams with `proxy: system`. |
| Any other | Read where the configuration writes `${NAME}`. |
