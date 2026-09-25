# 配置手册

[English](config.md)

ThinkWatch Core 只读一个文件：`config.yaml`。本文逐项说明其中每个字段的作用、默认值、可选值，以及改动如何进入正在运行的进程。在服务器上运行 core、由桌面应用远程管理，见[在服务器上运行 core](server.zh-CN.md)。

本文的字段表由代码生成，与代码不一致时测试失败。表中列出的字段，就是程序实际读取的字段。

## 文件位置

| 平台 | 默认位置 |
|---|---|
| macOS、Linux | `~/.thinkwatch/config.yaml` |
| Windows | `%APPDATA%\ThinkWatch\config.yaml` |

`THINKWATCH_HOME` 替换整个目录；`--config <路径>` 为单条命令指定文件。core 的其余数据也在这个目录里：请求数据库（`data.db`）、配置历史（`history/`）、下载的价目表（`model_prices.json`），以及本地控制通道的 socket 文件（`twcore.sock`；Windows 上是回环端口，记录在 `control.port` 中）。目录只有所有者可访问（`0700`），配置文件权限为 `0600`：其中以明文保存密钥。

没有配置文件时，`twcore serve` 会写入一份初始配置；`twcore init` 也可以按需生成。两者生成的内容如下：

```yaml
version: 1
listen:
  control:
    key: 6629…753d       # 自动生成
clients:
  - name: default
    key: tw-…            # 自动生成
```

这已是一份完整、合法的配置。其中还没有上游，因此控制面照常运行，请求会得到「尚未配置上游」的错误。加上一个上游即可转发：

```yaml
providers:
  - name: anthropic
    base_url: https://api.anthropic.com
    key: ${ANTHROPIC_API_KEY}
```

## 读取规则

- **本文没有的字段名一律是错误。**把 `port` 写成 `prot` 时，不会被悄悄忽略、让网关在默认端口上起来；整份配置被拒绝，错误信息指出那个字段。
- **默认值不写进文件。**没写的字段取下表中的默认值。应用和命令行只在取值不同于默认值时才写入字段，因此文件里出现的都是有人做出的选择。
- **`${VAR}` 读取环境变量**，适用于标注「可写 `${VAR}`」的字段：上游密钥、请求头的值、代理密码。取值来自 core 进程的环境，在发送请求时读取；变量未设置时，该上游的请求失败，错误信息指出变量名。在 systemd 下，这个环境就是 unit 的 `EnvironmentFile`。
- **名字即引用。**规则、策略组、密钥按名字引用上游、策略组、路由和价目表。指向不存在的名字，在加载时就报错，而不是成为一条永不命中的规则。在应用里改名时，所有引用在同一次写入中一起修改。以 `__` 开头的名字保留给内置项。
- **`version`** 是格式版本，目前为 `1`。版本更高的文件出自更新的 twcore，整份拒绝。

## 改动如何生效

修改配置有三种途径：桌面应用、`twcore config …` 命令、用编辑器直接修改文件。三者走同一条路径。core 监视配置文件，保存后一秒内重新加载，无需重启。

新版本要通过以下每一关才会换入：

1. 能按 YAML 解析。
2. 符合结构：字段名已知，类型正确。
3. 自洽：名字不重复、引用都能找到、正则能编译、CIDR 写法正确。
4. 能据此建立运行时对象。

任何一关失败，**原有配置继续服务**，错误信息说明失败在哪一关、哪个位置。写错一个字不会让网关停下。在保存出合法版本之前，桌面应用会一直显示这次拒绝。

`listen.gateway` 的改动同样即时生效：core 打开新的监听；打不开时（例如端口被占用）保留原监听并报告原因。保留期限的改动在下一次每小时的清理时生效。

两方同时修改时（应用和手工编辑），后写入的一方因版本不一致被拒绝，不会覆盖先写入的内容。

### 历史与回滚

每个生效过的版本都保存在配置文件旁边的 `history/` 目录中，并记录来源（应用、命令行、外部编辑、回滚、凭据轮换）。保留最近 50 个版本。

```sh
twcore check                      # 只校验配置，不启动任何服务
twcore config show                # 打印当前配置及其版本
twcore config history             # 列出历史版本，最新的在前
twcore config rollback 3f9a2c     # 回滚到某个版本（写前几位即可）
twcore config set /listen/gateway/port 8790 --int
```

这些命令直接操作文件，因此 core 没有运行时也能使用，而那往往正是最需要回滚的时候。正在运行的 core 会像对待其他保存一样接收这些改动。

`twcore config set <路径> <值>` 修改文件中已经写出的一个值。路径中的列表项按其 `name` 定位（`/providers/anthropic/base_url`）；数字表示下标（`/routes/0/rules/1/to`）。值默认按字符串写入，`--int`、`--bool`、`--null` 另作指定。写入前先校验结果。要添加文件中还没有的字段，请直接编辑文件。

### 网关写回的凭据

有一种写入不是由人发起的。上游的 OAuth token 端点换发新的 refresh token 后，旧的随即作废，因此网关会把新 token（以及 access token 和过期时间）写回 `providers[].oauth`。只改这几个值，注释和排版保持原样。

## 字段参考

每张表列出一节的全部字段。「默认值」一栏为「—」表示不写就没有这个字段，其含义见说明。

### 顶层

<!-- generated: table config -->
<a id="cfg-config"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `version` | 整数 | **必填** | 文件格式的版本，目前只有 `1`。更大的数字说明文件出自更新的 twcore，整份拒绝，不按一知半解的方式读。 |
| `listen` | 对象，见 [`listen`](#cfg-listen) | — | 网关和控制通道在哪里监听。 |
| `clients` | 对象列表，见 [`clients[]`](#cfg-clients) | `[]` | 网关密钥。至少要有一把；`twcore init` 和首次 `twcore serve` 会写入一把名为 `default` 的。 |
| `providers` | 对象列表，见 [`providers[]`](#cfg-providers) | `[]` | 上游。一个都没有也是合法配置：控制面照常运行，请求得到「尚未配置上游」的错误。 |
| `proxies` | 对象列表，见 [`proxies[]`](#cfg-proxies) | `[]` | 出站代理。在这里声明一次，由 `providers[].proxy` 按名字引用。 |
| `pricing` | 对象，见 [`pricing`](#cfg-pricing) | — | 默认价目表是否定期刷新，以及自定义价目表。 |
| `client_probes` | 对象，见 [`client_probes`](#cfg-client_probes) | — | 客户端自行发出的辅助请求（连通性检查、预热、起标题）如何处理。 |
| `security` | 对象，见 [`security`](#cfg-security) | — | 五项防护。出厂时都处在 `observe` 或 `off`，不改变、不拦截任何请求。 |
| `retention` | 对象，见 [`retention`](#cfg-retention) | — | 请求日志保留多久。 |
| `groups` | 对象列表，见 [`groups[]`](#cfg-groups) | `[]` | 策略组：多个上游合用一个名字，并规定如何在其中选择。 |
| `routes` | 对象列表，见 [`routes[]`](#cfg-routes) | `[]` | 路由。一条都不写时，请求按上游的声明顺序故障转移。 |
| `default_route` | 字符串 | — | 未指定路由的密钥走哪条路由。不写：名为 `default` 的路由；没有这条路由时走内置的故障转移。 |
| `default_key` | 字符串 | — | 没有专用密钥的客户端使用哪一把。不写：名为 `default` 的那把，没有则取第一把。这把密钥不能停用。 |
<!-- /generated -->

### `listen`

core 在哪里接受连接。连接分两种：客户端发送请求的 AI 网关，以及桌面应用和 `twcore` 命令使用的控制通道。

<!-- generated: table listen -->
<a id="cfg-listen"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `gateway` | 对象，见 [`listen.gateway`](#cfg-listen-gateway) | — | AI 网关，即客户端发送请求的地址。 |
| `control` | 对象，见 [`listen.control`](#cfg-listen-control) | — | 控制通道，即桌面应用和 `twcore` 命令连接 core 的途径。其中有控制密钥，因此每份配置都有这一节。 |
<!-- /generated -->

#### `listen.gateway`

<!-- generated: table listen.gateway -->
<a id="cfg-listen-gateway"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `bind` | `loopback` \| `all` \| 网卡名 \| IP 地址 | `loopback` | `loopback` 只有本机；`all` 所有网卡；网卡名（`en0`、`eth0`）每隔几秒重新解析，地址变了也能跟上，网卡暂时不在时先只监听 127.0.0.1，出现后再补上；写死的 IP 地址在地址变化后失效。绑定单张网卡时同时监听 127.0.0.1。 |
| `port` | 整数 | `8788` | 网关的 TCP 端口。 |
| `allow_from` | 字符串列表 | `[10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, fc00::/7]` | 本机以外允许连接的来源，写 CIDR 网段或单个地址。本机始终放行。`[]` 表示只有本机；放行所有来源要明确写 `0.0.0.0/0`。 |
<!-- /generated -->

`bind` 超出本机范围时，由 `allow_from` 决定允许谁连接。网关不提供 TLS：只在可信的网络中开放，或放在隧道、VPN 之后。

```yaml
listen:
  gateway:
    bind: all
    port: 8788
    allow_from: [192.168.1.0/24]
```

#### `listen.control`

控制通道供桌面应用，以及同一台机器上的 `twcore config`、`twcore control-key` 与 core 通信。本地通道是数据目录中的 socket 文件（Windows 上是回环端口）；除非启用 `remote`，控制通道不开任何网络端口。

每条控制连接，无论本地还是远程，都以一次握手开始，证明双方都持有 `key`（`Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`：密钥作为预共享密钥，每条连接协商新的会话密钥，通信内容加密）。不使用证书。不持有密钥的一方无法完成第一条握手消息。

<!-- generated: table listen.control -->
<a id="cfg-listen-control"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `key` | 字符串 | 自动生成 | 控制密钥：64 个十六进制字符（32 字节）。所有控制连接，无论本地还是远程，都要证明持有这把密钥。缺失时 `twcore serve` 在开始监听前写入一把；格式不对的配置整份拒绝。用 `twcore control-key` 查看，`twcore control-key --rotate` 更换。 |
| `remote` | 对象，见 [`listen.control.remote`](#cfg-listen-control-remote) | — | 供另一台机器上的桌面应用连接的网络端口。它是本地通道之外额外开的，不取代本地通道。 |
<!-- /generated -->

`key` 缺失时，由 `twcore serve` 在控制通道开始监听之前写入；`twcore init` 生成的配置也带有它。凡是显示配置或写入配置历史的地方，这个字段一律打码；把打码值原样存回时保留原值。密钥缺失或格式不对（不是 64 个十六进制字符）时整份配置无效，因此无法把它换成一把容易猜到的短密钥。

```sh
twcore control-key            # 显示密钥，用于粘贴到桌面应用
twcore control-key --rotate   # 更换密钥；已连接的应用需要重新连接
```

两条命令都在运行 core 的机器上执行。

#### `listen.control.remote`

供另一台机器上的桌面应用连接的网络端口。它在本地通道之外额外开启，因此这里写错（端口被占用、`allow_from` 把自己挡在外面）也不会把本机锁在门外：`twcore config` 和本机应用照常可用。

<!-- generated: table listen.control.remote -->
<a id="cfg-listen-control-remote"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `enabled` | 布尔 | `false` | 是否监听远程端口。不写或 `false`：不为控制面开任何网络端口。`twcore remote enable` / `twcore remote disable` 切换它；运行中的 core 在一秒内跟上。 |
| `bind` | `loopback` \| `all` \| 网卡名 \| IP 地址 | `all` | 监听哪张网卡，写法同 `listen.gateway.bind`。 |
| `port` | 整数 | **必填** | TCP 端口。没有固定默认值：`twcore init` 和 `twcore remote enable` 写出这一节时随机写入 20000 到 32000 之间的一个端口（不会和网关相同）。不能是 0，也不能和网关端口相同。 |
| `allow_from` | 字符串列表 | `[10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, fc00::/7]` | 允许连接的来源，写法同 `listen.gateway.allow_from`，但本机不会自动放行（本机有本地通道）。其他来源的连接在握手之前关闭，不回任何字节；收窄名单时，已经连着、不再放行的连接也随即断开。同一来源一分钟内握手失败 5 次，之后一分钟不理它。 |
<!-- /generated -->

```yaml
listen:
  control:
    key: 9f2c…e41a          # 64 个十六进制字符
    remote:
      enabled: true
      bind: all
      port: 23483            # 随机生成，在生成这一节时写入
      allow_from: [192.168.1.0/24]
```

经远程端口的连接无论应用如何请求，都不能做三件事：关闭 core（它由 systemd 管理）、修改 `listen.control`（它进来的那扇门）、生成诊断包（诊断包会写在服务器上）。握手限时 5 秒；同一来源一分钟内握手失败 5 次，封禁一分钟。

### `clients`

网关密钥，即 Claude Code、Codex 等客户端向网关发送的密钥。密钥即身份：并发上限、模型范围、路由都按密钥设置。

<!-- generated: table clients[] -->
<a id="cfg-clients"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `name` | 字符串 | **必填** | 密钥的名字，不能重复。路由规则用 `when.client` 匹配它。 |
| `key` | 字符串 | **必填** | 客户端发送的密钥（放在 `x-api-key` 或 `Authorization: Bearer` 中）。生成的密钥以 `tw-` 开头，以免被误认作上游的密钥。不能重复。 |
| `max_concurrent` | 整数 | — | 用这把密钥同时进行的请求数上限，超出的排队等待。不写：不限。`0` 会被拒绝。 |
| `allow` | 字符串列表 | — | 这把密钥可用的模型，写模型 ID 或通配（`claude-*`）。不写：全部模型。`[]`：一个都不给。 |
| `route` | 字符串 | — | 这把密钥的请求走哪条路由。不写：`default_route`。 |
| `client` | 字符串 | — | 这把密钥是为哪个客户端生成的（`claude-code`、`codex` 等），由桌面应用接管客户端时写入。一个客户端最多一把。 |
| `disabled` | 布尔 | `false` | 拒绝使用这把密钥的所有请求，密钥本身保留。 |
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

上游，即请求被转发到的接口。

<!-- generated: table providers[] -->
<a id="cfg-providers"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `name` | 字符串 | **必填** | 上游的名字，不能重复，也不能和策略组同名。以 `__` 开头的名字保留给内置项。 |
| `base_url` | 字符串 | **必填** | 接口地址，`http://` 或 `https://`，按服务商文档写到版本段为止（`https://api.anthropic.com`、`https://api.openai.com/v1`）。 |
| `key` | 字符串，可写 `${VAR}` | — | API 密钥，放进协议规定的请求头：`x-api-key`（Anthropic）、`Authorization: Bearer`（OpenAI）、`x-goog-api-key`（Gemini）。上游不需要密钥、或凭据写在 `headers` 里时不写。不能和 `oauth` 同时写。 |
| `headers` | 请求头名 → 值的映射 | `{}` | 额外的请求头，按书写顺序发送；值可以用 `${VAR}`，配置了 `oauth` 时可以用 `{{access_token}}`。最多 32 个。HTTP 或网关管理的请求头（`host`、`content-length`、`connection` 等）不能设置。 |
| `oauth` | 对象，见 [`providers[].oauth`](#cfg-providers-oauth) | — | OAuth 凭据：用 refresh token 换取 access token。与 `key` 二选一。 |
| `protocol` | `anthropic` \| `openai-chat` \| `openai-responses` \| `gemini` \| `chatgpt` | — | 上游的接口格式。不写：官方地址按 `base_url` 识别，其余按 `anthropic` 处理。 |
| `proxy` | 字符串 | `direct` | `direct`；`system`，即 core 进程环境变量 `HTTPS_PROXY`、`HTTP_PROXY`、`ALL_PROXY` 中的代理；或 `proxies` 中某一项的名字。 |
| `on_proxy_fail` | `fail` \| `direct` | `fail` | 代理不可用时：请求失败（`fail`），或改为直连（`direct`）。 |
| `models` | 字符串列表 | `[]` | 上游不支持 `/v1/models` 时，按这份清单认定它提供的模型。 |
| `models_only` | 字符串列表 | — | 只使用这家的这些模型，写 ID 或通配。范围外的模型不出现在模型列表里，也不会路由到这家。不写：全部。写空列表会被拒绝，暂停使用请用 `disabled`。 |
| `billing` | `per-token` \| `free` | `per-token` | `per-token`：费用为用量乘以所选价目表中的单价，订阅账号同样如此。`free`：费用记为 0。 |
| `pricing` | 字符串 | — | `pricing.sheets` 中某张价目表的名字。不写：默认价目表。 |
| `disabled` | 布尔 | `false` | 不参与路由，模型也不出现在模型列表里；配置原样保留。 |
<!-- /generated -->

凭据有三种写法：`key`，放进协议规定的请求头；`oauth`，用 refresh token 换取 token；`headers`，用于上游自有的鉴权方式。`headers` 可以和前两者同时使用，但不能再设置已经承载凭据的那个请求头。

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

ChatGPT 账号上游（`protocol: chatgpt`）只接受桌面应用登录得到的凭据，不能手写。不支持 Claude 和 Google 的订阅登录，请使用 API 密钥。

#### `providers[].oauth`

<!-- generated: table providers[].oauth -->
<a id="cfg-providers-oauth"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `access` | 字符串 | — | 当前的 access token。每次刷新后由网关写回；不写则在第一次使用时换取。 |
| `expires_at` | 字符串 | — | `access` 的过期时间，RFC 3339（UTC），随 token 一起写回。不写：一直用到上游返回 401。 |
| `refresh` | 字符串 | **必填** | Refresh token。token 端点换发新的之后旧的即作废，因此网关会把新的写回本文件。 |
| `endpoint` | 字符串 | **必填** | token 端点的地址。 |
| `client_id` | 字符串 | — | OAuth 客户端 ID，端点需要时填写。 |
| `client_secret` | 字符串 | — | OAuth 客户端密钥，端点需要时填写。 |
| `refresh_before` | 时长（`30s`、`5m`、`1h`） | — | 提前多久刷新。不写或写法无法识别：`5m`。 |
<!-- /generated -->

access token 默认放进协议的鉴权请求头。要放在别处，在 `headers` 中写出那个请求头，用 `{{access_token}}` 标出 token 的位置：

```yaml
    oauth:
      refresh: ${VENDOR_REFRESH_TOKEN}
      endpoint: https://auth.example.com/oauth/token
      client_id: my-client
    headers:
      X-Access: Token {{access_token}}
```

### `proxies`

出站代理。不同上游需要的代理往往不同，因此没有全局开关：由每个上游用 `proxy` 选择。

<!-- generated: table proxies[] -->
<a id="cfg-proxies"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `name` | 字符串 | **必填** | `providers[].proxy` 引用的名字。`direct` 和 `system` 是内置的。 |
| `type` | `socks5` \| `socks5h` \| `http` \| `https` | `socks5h` | `socks5h` 把域名交给代理解析；`socks5` 先在本地解析。`http` 和 `https` 是 HTTP 代理。 |
| `addr` | 字符串 | **必填** | 代理的 `host:port`。 |
| `auth` | 对象，见 [`proxies[].auth`](#cfg-proxies-auth) | — | 代理需要时填写用户名和密码。 |
<!-- /generated -->

#### `proxies[].auth`

<!-- generated: table proxies[].auth -->
<a id="cfg-proxies-auth"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `user` | 字符串 | **必填** | 用户名。 |
| `pass` | 字符串，可写 `${VAR}` | **必填** | 密码。 |
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

`on_proxy_fail` 默认为 `fail`：静默改为直连会让请求走一条意料之外的路径，而使用者仍以为请求经过了代理。

### `pricing`

一次请求的费用为用量乘以模型单价。单价来自默认价目表（LiteLLM 的公开数据集，程序内置一份，每天联网刷新），或来自上游选用的自定义价目表。单价变动只影响此后的请求，不改变已记录请求的费用。

<!-- generated: table pricing -->
<a id="cfg-pricing"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `auto_update` | 布尔 | `true` | 每天联网刷新一次默认价目表，保存为 `config.yaml` 旁边的 `model_prices.json`；此前以及离线时使用内置于程序中的价目表。 |
| `sheets` | 对象列表，见 [`pricing.sheets[]`](#cfg-pricing-sheets) | `[]` | 自定义价目表。上游用 `providers[].pricing` 选用。 |
<!-- /generated -->

#### `pricing.sheets`

<!-- generated: table pricing.sheets[] -->
<a id="cfg-pricing-sheets"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `name` | 字符串 | **必填** | 价目表的名字，不能重复。 |
| `multiplier` | 数字 | `1` | 作用于默认价目表的全部单价，包括缓存和长上下文单价。 |
| `models` | 映射： 模型 ID → [`pricing.sheets[].models.*`](#cfg-pricing-sheets-models) | `{}` | 单独定价的模型。它们取代默认价目表中该模型的单价，不乘倍率。 |
<!-- /generated -->

#### `pricing.sheets[].models`

单价以每百万 token 的美元计，与厂商价格页上的写法一致。每个字段都要写明，计价时不做任何推算。

<!-- generated: table pricing.sheets[].models.* -->
<a id="cfg-pricing-sheets-models"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `input` | 数字 | **必填** | 每百万输入 token 的美元价格。 |
| `output` | 数字 | **必填** | 每百万输出 token 的美元价格。 |
| `cache_read` | 数字 | **必填** | 每百万缓存读取 token 的美元价格。 |
| `cache_write_5m` | 数字 | **必填** | 每百万写入 5 分钟缓存 token 的美元价格。 |
| `cache_write_1h` | 数字 | **必填** | 每百万写入 1 小时缓存 token 的美元价格。 |
| `input_above_200k` | 数字 | — | 单次请求输入超过 200K token 后的输入单价。与 `output_above_200k` 同时写或都不写。 |
| `output_above_200k` | 数字 | — | 单次请求输入超过 200K token 后的输出单价。 |
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

客户端发出的请求中，有一部分并非出自使用者：连通性检查、预热、会话标题、话题检测、建议。每一类都可以在本地应答（`intercept`，不向上游发送任何内容）、原样放行（`passthrough`），或交给路由规则（`route`，由 `when.intent` 匹配）。默认只拦下拦了也不会少任何东西的那几类。

<!-- generated: table client_probes -->
<a id="cfg-client_probes"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `health_check` | `intercept` \| `passthrough` \| `route` | `intercept` | 连通性检查（`max_tokens: 1`）。默认在本地应答，不影响任何功能。 |
| `warmup` | `intercept` \| `passthrough` \| `route` | `intercept` | 预热请求。默认在本地应答。 |
| `titling` | `intercept` \| `passthrough` \| `route` | `passthrough` | 为会话起标题的请求。默认放行：拦下后所有会话都会是同一个标题。 |
| `topic_detect` | `intercept` \| `passthrough` \| `route` | `passthrough` | 话题检测。默认放行。 |
| `suggestion` | `intercept` \| `passthrough` \| `route` | `passthrough` | 建议。默认放行。 |
<!-- /generated -->

### `security`

五项防护，对所有上游一视同仁。每一项都有 `mode`：`off`、`observe`（检测并记录，不改变任何行为）、`enforce`（处置）。出厂时除输出长度为 `off` 外，其余都是 `observe`。各项在 `enforce` 下的处置不同，分别见下文。

<!-- generated: table security -->
<a id="cfg-security"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `redact` | 对象，见 [`security.redact`](#cfg-security-redact) | — | 出站脱敏：请求发出前，把其中的凭据替换掉。 |
| `inspect_tools` | 对象，见 [`security.inspect_tools`](#cfg-security-inspect_tools) | — | 工具调用审查：模型返回的工具调用中出现危险命令时切断响应。 |
| `hidden_text` | 对象，见 [`security.hidden_text`](#cfg-security-hidden_text) | — | 人看不见、模型读得到的隐藏字符，出现时拒绝请求。 |
| `content` | 对象，见 [`security.content`](#cfg-security-content) | — | 内容过滤：调用方发送的内容中出现指定的词或写法时拒绝请求。 |
| `output_limit` | 对象，见 [`security.output_limit`](#cfg-security-output_limit) | — | 输出长度：回答超过上限时切断。 |
<!-- /generated -->

#### `security.redact`

请求发出前查找其中的凭据。`enforce` 下将其替换。

<!-- generated: table security.redact -->
<a id="cfg-security-redact"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `mode` | `off` \| `observe` \| `enforce` | `observe` | `off` 不检测；`observe` 检测并记录，不改变任何行为；`enforce` 检测并处置。 |
| `enable` | 字符串列表 | `[]` | 打开出厂时关着的内置规则，按 id。 |
| `disable` | 字符串列表 | `[]` | 关掉内置规则，按 id。 |
| `custom` | 对象列表，见 [`security.redact.custom[]`](#cfg-security-redact-custom) | `[]` | 自定义规则：正则匹配到的内容按凭据处理。 |
<!-- /generated -->

<!-- generated: table security.redact.custom[] -->
<a id="cfg-security-redact-custom"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `name` | 字符串 | **必填** | 日志和应用里显示的名字，也是规则的标识；同一项防护里不能重名。 |
| `pattern` | 字符串 | **必填** | 正则表达式。 |
| `disabled` | 布尔 | `false` | 停用这条规则，规则本身留在文件里。 |
<!-- /generated -->

内置规则：

<!-- generated: rules redact -->
| id | 名称 | 出厂 |
|---|---|---|
| `anthropic-api-key` | Anthropic API key | 开 |
| `openai-project-key` | OpenAI project key | 开 |
| `openai-api-key` | OpenAI API key | 开 |
| `github-personal-token` | GitHub personal access token | 开 |
| `github-oauth-token` | GitHub OAuth token | 开 |
| `github-server-token` | GitHub server token | 开 |
| `github-user-token` | GitHub user token | 开 |
| `github-fine-grained-token` | GitHub fine-grained token | 开 |
| `slack-bot-token` | Slack bot token | 开 |
| `slack-user-token` | Slack user token | 开 |
| `slack-app-token` | Slack app token | 开 |
| `aws-access-key-id` | AWS access key ID | 开 |
| `aws-temporary-key-id` | AWS temporary access key ID | 开 |
| `google-api-key` | Google API key | 开 |
| `google-oauth-token` | Google OAuth token | 开 |
| `gitlab-token` | GitLab token | 开 |
| `stripe-live-key` | Stripe live key | 开 |
| `stripe-restricted-key` | Stripe restricted key | 开 |
| `npm-token` | npm token | 开 |
| `digitalocean-token` | DigitalOcean token | 开 |
| `sendgrid-key` | SendGrid key | 开 |
| `private-key` | Private key | 开 |
| `jwt` | JWT | 开 |
| `conn-string-password` | Connection string password | 开 |
| `internal-ip` | Internal IP address | 关 |
| `internal-domain` | Internal domain | 关 |
<!-- /generated -->

#### `security.inspect_tools`

按规则检查模型返回的工具调用。`enforce` 下命中处置为 `cut` 的规则时切断响应，客户端拿不到可执行的完整调用。

<!-- generated: table security.inspect_tools -->
<a id="cfg-security-inspect_tools"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `mode` | `off` \| `observe` \| `enforce` | `observe` | `off` 不检测；`observe` 检测并记录，不改变任何行为；`enforce` 检测并处置。 |
| `enable` | 字符串列表 | `[]` | 打开出厂时关着的内置规则，按 id。 |
| `disable` | 字符串列表 | `[]` | 关掉内置规则，按 id。 |
| `actions` | 映射： 内置规则 id → `cut` \| `record` | `{}` | 内置规则在 `enforce` 下的处置，只写与出厂不同的（`rm-rf-root: record`）。 |
| `custom` | 对象列表，见 [`security.inspect_tools.custom[]`](#cfg-security-inspect_tools-custom) | `[]` | 自定义规则，按工具调用的参数匹配。 |
<!-- /generated -->

<!-- generated: table security.inspect_tools.custom[] -->
<a id="cfg-security-inspect_tools-custom"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `name` | 字符串 | **必填** | 日志和应用里显示的名字，也是规则的标识；同一项防护里不能重名。 |
| `pattern` | 字符串 | **必填** | 正则表达式。 |
| `action` | `cut` \| `record` | `record` | `enforce` 下切断响应（`cut`），或只记录（`record`）。 |
| `disabled` | 布尔 | `false` | 停用这条规则，规则本身留在文件里。 |
<!-- /generated -->

内置规则：

<!-- generated: rules inspect_tools -->
| id | 名称 | `enforce` 下出厂处置 |
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

调用方发送的内容中（包括工具结果）人看不见、模型读得到的字符。`enforce` 下拒绝请求。

<!-- generated: table security.hidden_text -->
<a id="cfg-security-hidden_text"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `mode` | `off` \| `observe` \| `enforce` | `observe` | `off` 不检测；`observe` 检测并记录，不改变任何行为；`enforce` 检测并处置。 |
| `disable` | 字符串列表 | `[]` | 不检查的种类：`tag`、`bidi`。 |
<!-- /generated -->

<!-- generated: rules hidden_text -->
| 种类 | 说明 |
|---|---|
| `tag` | Unicode 标签字符（U+E0000 至 U+E007F）：在任何地方都不可见，模型却能读到，足以藏下一整段指令。 |
| `bidi` | 双向控制符：使显示顺序与模型读到的顺序不一致。 |
<!-- /generated -->

#### `security.content`

调用方发送的内容中出现的词或写法。`enforce` 下命中处置为 `block` 的规则时拒绝请求。

<!-- generated: table security.content -->
<a id="cfg-security-content"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `mode` | `off` \| `observe` \| `enforce` | `observe` | `off` 不检测；`observe` 检测并记录，不改变任何行为；`enforce` 检测并处置。 |
| `enable` | 字符串列表 | `[]` | 打开出厂时关着的内置规则，按 id。 |
| `disable` | 字符串列表 | `[]` | 关掉内置规则，按 id。 |
| `actions` | 映射： 内置规则 id → `block` \| `record` | `{}` | 内置规则在 `enforce` 下的处置，只写与出厂不同的。 |
| `custom` | 对象列表，见 [`security.content.custom[]`](#cfg-security-content-custom) | `[]` | 自定义规则。 |
<!-- /generated -->

<!-- generated: table security.content.custom[] -->
<a id="cfg-security-content-custom"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `name` | 字符串 | **必填** | 日志和应用里显示的名字，也是规则的标识；同一项防护里不能重名。 |
| `pattern` | 字符串 | **必填** | 关键词；`match: regex` 时为正则表达式。均不区分大小写。 |
| `match` | `contains` \| `regex` | `contains` | `contains`：正文包含 `pattern`。`regex`：`pattern` 是正则表达式。 |
| `action` | `block` \| `record` | `record` | `enforce` 下拒绝请求（`block`），或只记录（`record`）。 |
| `disabled` | 布尔 | `false` | 停用这条规则，规则本身留在文件里。 |
<!-- /generated -->

内置规则：

<!-- generated: rules content -->
| id | 名称 | 分组 | 出厂 | `enforce` 下出厂处置 |
|---|---|---|---|---|
| `ignore-previous-instructions` | Ignore previous instructions | injection | 开 | `block` |
| `ignore-all-previous` | Ignore all previous | injection | 开 | `block` |
| `disregard-your-instructions` | Disregard your instructions | injection | 开 | `block` |
| `jailbreak` | Jailbreak | injection | 关 | `block` |
| `dan` | DAN | injection | 关 | `block` |
| `developer-mode` | Developer mode | injection | 关 | `block` |
| `you-are-now` | Persona manipulation | persona | 关 | `block` |
| `new-persona` | New persona | persona | 关 | `record` |
| `act-as` | Act as | persona | 关 | `record` |
| `pretend-to-be` | Pretend to be | persona | 关 | `record` |
| `system-prompt` | System prompt extraction | persona | 关 | `record` |
| `reveal-your-instructions` | Reveal instructions | persona | 关 | `record` |
| `what-are-your-rules` | What are your rules | persona | 关 | `record` |
| `base64-wall` | Base64 smuggling | persona | 关 | `record` |
| `zh-ignore-previous` | Ignore previous instructions (Chinese) | chinese | 关 | `block` |
| `zh-forget-your` | Forget your instructions (Chinese) | chinese | 关 | `block` |
| `zh-do-not-follow` | Do not follow (Chinese) | chinese | 关 | `block` |
| `zh-you-are-now` | You are now (Chinese) | chinese | 关 | `block` |
| `zh-role-play` | Role-play (Chinese) | chinese | 关 | `record` |
| `zh-reveal-your` | Reveal your instructions (Chinese) | chinese | 关 | `record` |
| `zh-system-prompt` | System prompt (Chinese) | chinese | 关 | `record` |
| `zh-jailbreak` | Jailbreak (Chinese) | chinese | 关 | `block` |
<!-- /generated -->

#### `security.output_limit`

<!-- generated: table security.output_limit -->
<a id="cfg-security-output_limit"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `mode` | `off` \| `observe` \| `enforce` | `off` | 出厂关闭：没有一个上限适合所有用途。`observe` 记录超长的回答；`enforce` 在超过上限处停止输出。 |
| `max_chars` | 整数 | `100000` | 上限，按字符（Unicode 标量）计，取值 1 到 1000000。 |
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

设两个期限，是因为两类数据的体积相差三个数量级：一条请求的正文有几十 KB，一条请求记录只有几百字节。字节上限用于应对用量突增。

<!-- generated: table retention -->
<a id="cfg-retention"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `body_days` | 整数 | `7` | 请求和响应正文保留的天数。 |
| `row_days` | 整数 | `90` | 每条请求记录（时间、模型、用量、费用）保留的天数。 |
| `body_max_bytes` | 整数 | `2147483648` | 正文最多占用的字节数，超出时从最早的日期开始删除。默认 2 GiB。 |
<!-- /generated -->

### `groups`

策略组让多个上游合用一个名字。规则用 `to` 把请求交给策略组。

<!-- generated: table groups[] -->
<a id="cfg-groups"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `name` | 字符串 | **必填** | 策略组的名字，不能重复，也不能和上游同名。 |
| `type` | `fallback` \| `select` \| `load-balance` \| `url-test` \| `cheapest` | `fallback` | `fallback`：按顺序取第一个健康的。`select`：取 `selected` 指定的那个。`load-balance`：轮流。`url-test`：按实测首字节时间取最快的。`cheapest`：取输入单价最低的。 |
| `providers` | 字符串列表 | **必填** | 成员上游的名字。 |
| `session_affinity` | 布尔 | `true` | 同一会话固定走同一家，使 prompt cache 持续命中。在 `load-balance` 下关闭会让每一轮都换一家，缓存随之失效。 |
| `selected` | 字符串 | — | `select` 类型选中的成员。 |
<!-- /generated -->

默认类型为 `fallback`：把一个会话分散到多家上游会丢掉 prompt cache，而在单个使用者的机器上，分散负载换来的远不及缓存省下的。

### `routes`

一条路由是一组自上而下求值的规则。每把密钥使用其 `route` 指定的路由；没有指定时用 `default_route`；再没有时用名为 `default` 的路由；一条路由都没有时，请求按上游的声明顺序故障转移。

<!-- generated: table routes[] -->
<a id="cfg-routes"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `name` | 字符串 | **必填** | 路由的名字，不能重复。`default` 是密钥默认使用的那条。 |
| `rules` | 对象列表，见 [`routes[].rules[]`](#cfg-routes-rules) | `[]` | 自上而下求值；第一条匹配且带有 `to` 或 `deny` 的规则决定请求去向。 |
<!-- /generated -->

#### `routes[].rules`

<!-- generated: table routes[].rules[] -->
<a id="cfg-routes-rules"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `name` | 字符串 | **必填** | 日志和流量详情中显示的名字。 |
| `when` | 对象，见 [`routes[].rules[].when`](#cfg-routes-rules-when) | — | 条件，须全部满足。不写：匹配所有请求。 |
| `to` | 字符串 | — | 上游或策略组的名字；`__all__` 表示按声明顺序的全部上游。不能与 `when.provider_would_be` 同时写。 |
| `set` | 对象，见 [`routes[].rules[].set`](#cfg-routes-rules-set) | — | 改写请求参数。从所有匹配的规则累积，不只第一条。 |
| `deny` | 字符串 | — | 以这句原因拒绝请求。 |
<!-- /generated -->

#### `routes[].rules[].when`

<!-- generated: table routes[].rules[].when -->
<a id="cfg-routes-rules-when"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `model` | 字符串 | — | 请求的模型，可用通配（`claude-opus-*`）。 |
| `client` | 字符串 | — | 请求所用网关密钥的名字，精确匹配。 |
| `dialect` | 字符串 | — | 客户端使用的接口格式：`anthropic`、`openai-chat`、`openai-responses`、`gemini`。 |
| `input_tokens` | 比较式（`>200k`、`<=4k`、`==3`） | — | 估算的输入 token 数。 |
| `max_tokens` | 比较式（`>200k`、`<=4k`、`==3`） | — | 请求中的 `max_tokens`。未写该参数的请求不匹配。 |
| `tool_count` | 比较式（`>200k`、`<=4k`、`==3`） | — | 请求中提供的工具数量。 |
| `cache` | 布尔 | — | 请求是否使用 prompt cache。 |
| `tools` | 布尔 | — | 请求是否带工具。 |
| `image` | 布尔 | — | 请求是否包含图片。 |
| `thinking` | 布尔 | — | 是否开启扩展思考。 |
| `stream` | 布尔 | — | 是否流式返回。 |
| `intent` | 字符串或字符串列表 | — | 客户端的辅助请求：`assistant_internal` 表示任意一类，也可以写具体的一类（`titling`）。只有在 `client_probes` 中设为 `route` 的类别才会进入路由。 |
| `provider_would_be` | 字符串或字符串列表 | — | 路由选中的上游。这类规则在路由完成后求值，只能 `set` 或 `deny`，不能写 `to`。 |
<!-- /generated -->

比较式以 `>`、`>=`、`<`、`<=` 或 `==` 开头，数字可以带 `k` 或 `m` 后缀：`">200k"`、`"<=4k"`。不带运算符是错误，不当作相等：单写 `"200k"` 会被拒绝。

#### `routes[].rules[].set`

<!-- generated: table routes[].rules[].set -->
<a id="cfg-routes-rules-set"></a>

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `model` | 字符串 | — | 换成另一个模型发送。该会话的 prompt cache 随之失效。 |
| `max_tokens` | 整数 | — | 替换 `max_tokens`。 |
| `thinking` | 布尔 | — | 开启或关闭扩展思考。 |
| `only_at_session_start` | 布尔 | `false` | 只在会话开始时应用。目前只记录和显示，尚未生效。 |
<!-- /generated -->

```yaml
groups:
  - name: fast
    type: url-test
    providers: [anthropic, relay]

routes:
  - name: default
    rules:
      - name: 长上下文走官方
        when: { input_tokens: ">200k" }
        to: anthropic
      - name: 标题用便宜模型
        when: { intent: titling }
        set: { model: claude-haiku-4-5 }
      - name: 其余
        to: fast
default_route: default
```

## 环境变量

| 变量 | 作用 |
|---|---|
| `THINKWATCH_HOME` | 数据目录，替代 `~/.thinkwatch`（Windows 上为 `%APPDATA%\ThinkWatch`）。 |
| `TWCORE_LOG` | 日志过滤，`tracing` 语法（`info`、`debug`、`tw_gateway=debug`）。 |
| `HTTPS_PROXY`、`HTTP_PROXY`、`ALL_PROXY`、`NO_PROXY` | 供 `proxy: system` 的上游使用。 |
| 其他 | 配置中写 `${NAME}` 的位置读取。 |
