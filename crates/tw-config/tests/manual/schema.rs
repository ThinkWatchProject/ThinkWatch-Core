//! config.yaml 的每一节、每一个字段，手册里那张表就是照这里渲染的。
//!
//! **改了 tw-config（或者它引用的 tw-engine、tw-pricing）的配置类型，就改这里**：
//! 加一行、删一行、改默认值。`manual.rs` 会拿这份声明和代码逐项对，对不上时
//! 说清楚是哪个字段、该怎么改。改完用 `UPDATE_CONFIG_DOCS=1` 重新生成手册。
//!
//! 说明写给手写配置文件的人：这个字段管什么、不写是什么意思、写错了会怎样。
//! 两种语言各写一遍，**不是互译的字面对照**，各自按各自的习惯说。

use super::{Def, Kind, Lang, Row, Section, T2};
use tw_config::proxy::ProxyAuth;
use tw_config::*;
use tw_engine::rule::When;
use tw_engine::{Group, GroupType, RouteSet, Rule, SetAction};
use tw_pricing::{PerMillion, PricingConfig, SheetDef};

const fn t(en: &'static str, zh: &'static str) -> T2 {
    T2 { en, zh }
}

const fn row(name: &'static str, kind: Kind, def: Def, doc: T2) -> Row {
    Row {
        name,
        kind,
        def,
        doc,
    }
}

// 枚举的取值问 serde 要。函数指针要一个具体的函数，所以一个类型一个
fn protocols() -> Vec<&'static str> {
    super::fields::<Protocol>()
}
fn billings() -> Vec<&'static str> {
    super::fields::<Billing>()
}
fn proxy_kinds() -> Vec<&'static str> {
    super::fields::<ProxyKind>()
}
fn on_proxy_fail() -> Vec<&'static str> {
    super::fields::<OnProxyFail>()
}
fn probe_actions() -> Vec<&'static str> {
    super::fields::<ProbeAction>()
}
fn modes() -> Vec<&'static str> {
    super::fields::<SecurityMode>()
}
fn tool_actions() -> Vec<&'static str> {
    super::fields::<ToolAction>()
}
fn content_actions() -> Vec<&'static str> {
    super::fields::<ContentAction>()
}
fn content_matches() -> Vec<&'static str> {
    super::fields::<ContentMatch>()
}
fn group_types() -> Vec<&'static str> {
    super::fields::<GroupType>()
}

const RULE_ID: T2 = t("built-in rule id", "内置规则 id");
const MODE_DOC: T2 = t(
    "`off` does nothing; `observe` detects and records only, and changes nothing; `enforce` \
     detects and acts.",
    "`off` 不检测；`observe` 检测并记录，不改变任何行为；`enforce` 检测并处置。",
);
const ENABLE_DOC: T2 = t(
    "Built-in rules to switch on that are off out of the box, by id.",
    "打开出厂时关着的内置规则，按 id。",
);
const DISABLE_DOC: T2 = t(
    "Built-in rules to switch off, by id.",
    "关掉内置规则，按 id。",
);
const RULE_NAME: T2 = t(
    "Name shown in logs and in the app; it identifies the rule and has to be unique within this \
     guard.",
    "日志和应用里显示的名字，也是规则的标识；同一项防护里不能重名。",
);
const RULE_DISABLED: T2 = t(
    "Switches the rule off and keeps it in the file.",
    "停用这条规则，规则本身留在文件里。",
);

/// 手册里要有的内置规则清单，`<!-- generated: rules … -->`。
pub const RULE_LISTS: &[&str] = &["redact", "inspect_tools", "hidden_text", "content"];

pub fn sections() -> Vec<Section> {
    vec![
        // ── 顶层 ──────────────────────────────────────────────
        Section {
            path: "config",
            ty: checked!(Config, "version: 1"),
            rows: vec![
                row(
                    "version",
                    Kind::Int,
                    Def::Required,
                    t(
                        "Format version of this file. The only version is `1`. A file with a higher number was written by a newer twcore and is refused rather than half-understood.",
                        "文件格式的版本，目前只有 `1`。更大的数字说明文件出自更新的 twcore，整份拒绝，不按一知半解的方式读。",
                    ),
                ),
                row(
                    "listen",
                    Kind::Obj("listen"),
                    Def::Section,
                    t(
                        "Where the gateway and the control channel listen.",
                        "网关和控制通道在哪里监听。",
                    ),
                ),
                row(
                    "clients",
                    Kind::Objs("clients[]"),
                    Def::Is("[]"),
                    t(
                        "Gateway keys. At least one is required; `twcore init` and the first `twcore serve` write one named `default`.",
                        "网关密钥。至少要有一把；`twcore init` 和首次 `twcore serve` 会写入一把名为 `default` 的。",
                    ),
                ),
                row(
                    "providers",
                    Kind::Objs("providers[]"),
                    Def::Is("[]"),
                    t(
                        "Upstreams. None is a valid configuration: the control plane runs and requests are answered with an error saying no upstream is configured.",
                        "上游。一个都没有也是合法配置：控制面照常运行，请求得到「尚未配置上游」的错误。",
                    ),
                ),
                row(
                    "proxies",
                    Kind::Objs("proxies[]"),
                    Def::Is("[]"),
                    t(
                        "Outbound proxies, declared once and referred to by name from `providers[].proxy`.",
                        "出站代理。在这里声明一次，由 `providers[].proxy` 按名字引用。",
                    ),
                ),
                row(
                    "pricing",
                    Kind::Obj("pricing"),
                    Def::Section,
                    t(
                        "Refreshing the default price table, and price sheets of your own.",
                        "默认价目表是否定期刷新，以及自定义价目表。",
                    ),
                ),
                row(
                    "client_probes",
                    Kind::Obj("client_probes"),
                    Def::Section,
                    t(
                        "What happens to the helper requests clients send on their own (health checks, warm-ups, titles).",
                        "客户端自行发出的辅助请求（连通性检查、预热、起标题）如何处理。",
                    ),
                ),
                row(
                    "security",
                    Kind::Obj("security"),
                    Def::Section,
                    t(
                        "The five guards. All of them start in `observe` or `off`, so out of the box nothing is changed or blocked.",
                        "五项防护。出厂时都处在 `observe` 或 `off`，不改变、不拦截任何请求。",
                    ),
                ),
                row(
                    "retention",
                    Kind::Obj("retention"),
                    Def::Section,
                    t("How long request logs are kept.", "请求日志保留多久。"),
                ),
                row(
                    "groups",
                    Kind::Objs("groups[]"),
                    Def::Is("[]"),
                    t(
                        "Strategy groups: several upstreams behind one name, with a way to pick among them.",
                        "策略组：多个上游合用一个名字，并规定如何在其中选择。",
                    ),
                ),
                row(
                    "routes",
                    Kind::Objs("routes[]"),
                    Def::Is("[]"),
                    t(
                        "Routes. Without any, requests fail over across all upstreams in the order they are declared.",
                        "路由。一条都不写时，请求按上游的声明顺序故障转移。",
                    ),
                ),
                row(
                    "default_route",
                    Kind::Str,
                    Def::Unset,
                    t(
                        "The route for keys that do not name one. Unset: the route named `default`, or the built-in failover when there is none.",
                        "未指定路由的密钥走哪条路由。不写：名为 `default` 的路由；没有这条路由时走内置的故障转移。",
                    ),
                ),
                row(
                    "default_key",
                    Kind::Str,
                    Def::Unset,
                    t(
                        "The gateway key for clients that were not given a key of their own. Unset: the key named `default`, or the first key. It cannot be disabled.",
                        "没有专用密钥的客户端使用哪一把。不写：名为 `default` 的那把，没有则取第一把。这把密钥不能停用。",
                    ),
                ),
            ],
        },
        // ── listen ────────────────────────────────────────────
        Section {
            path: "listen",
            ty: checked!(Listen, "{}"),
            rows: vec![
                row(
                    "gateway",
                    Kind::Obj("listen.gateway"),
                    Def::Section,
                    t(
                        "The AI gateway: the address clients send requests to.",
                        "AI 网关，即客户端发送请求的地址。",
                    ),
                ),
                row(
                    "control",
                    Kind::Obj("listen.control"),
                    Def::Section,
                    t(
                        "The control channel: how the desktop app and `twcore` commands reach core. It holds the control key, so every configuration has it.",
                        "控制通道，即桌面应用和 `twcore` 命令连接 core 的途径。其中有控制密钥，因此每份配置都有这一节。",
                    ),
                ),
            ],
        },
        Section {
            path: "listen.gateway",
            ty: checked!(GatewayListen, "{}"),
            rows: vec![
                row(
                    "bind",
                    Kind::Bind,
                    Def::Is("loopback"),
                    t(
                        "`loopback` is this machine only; `all` is every interface; an interface name (`en0`, `eth0`) is looked up at start and follows address changes; a fixed IP address stops working when the address changes. Binding one interface also listens on 127.0.0.1.",
                        "`loopback` 只有本机；`all` 所有网卡；网卡名（`en0`、`eth0`）在启动时解析，地址变了也能跟上；写死的 IP 地址在地址变化后失效。绑定单张网卡时同时监听 127.0.0.1。",
                    ),
                ),
                row(
                    "port",
                    Kind::Int,
                    Def::Is("8788"),
                    t("TCP port of the gateway.", "网关的 TCP 端口。"),
                ),
                row(
                    "allow_from",
                    Kind::Strs,
                    Def::Is("[10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, fc00::/7]"),
                    t(
                        "Sources other than this machine that may connect, as CIDR ranges or single addresses. This machine is always allowed. `[]` means this machine only; `0.0.0.0/0` allows everyone and has to be written out.",
                        "本机以外允许连接的来源，写 CIDR 网段或单个地址。本机始终放行。`[]` 表示只有本机；放行所有来源要明确写 `0.0.0.0/0`。",
                    ),
                ),
            ],
        },
        Section {
            path: "listen.control",
            ty: checked!(ControlListen, "{}"),
            rows: vec![
                row(
                    "key",
                    Kind::Str,
                    Def::Said(t("generated", "自动生成")),
                    t(
                        "The control key: 64 hexadecimal characters (32 bytes). Every control connection, local or remote, proves it knows this key. `twcore serve` writes one before listening if it is missing; a configuration where it is malformed is refused. Show it with `twcore control-key`, replace it with `twcore control-key --rotate`.",
                        "控制密钥：64 个十六进制字符（32 字节）。所有控制连接，无论本地还是远程，都要证明持有这把密钥。缺失时 `twcore serve` 在开始监听前写入一把；格式不对的配置整份拒绝。用 `twcore control-key` 查看，`twcore control-key --rotate` 更换。",
                    ),
                ),
                row(
                    "remote",
                    Kind::Obj("listen.control.remote"),
                    Def::Section,
                    t(
                        "A network port for the desktop app on another machine. Additional to the local channel, never instead of it.",
                        "供另一台机器上的桌面应用连接的网络端口。它是本地通道之外额外开的，不取代本地通道。",
                    ),
                ),
            ],
        },
        Section {
            path: "listen.control.remote",
            ty: checked!(RemoteListen, "{port: 20000}"),
            rows: vec![
                row(
                    "enabled",
                    Kind::Bool,
                    Def::Is("false"),
                    t(
                        "Listen on the remote port. Unset or `false`: no network port is opened for control. `twcore remote enable` and `twcore remote disable` switch it; a running core follows within a second.",
                        "是否监听远程端口。不写或 `false`：不为控制面开任何网络端口。`twcore remote enable` / `twcore remote disable` 切换它；运行中的 core 在一秒内跟上。",
                    ),
                ),
                row(
                    "bind",
                    Kind::Bind,
                    Def::Is("all"),
                    t(
                        "Interface to listen on, written as for `listen.gateway.bind`.",
                        "监听哪张网卡，写法同 `listen.gateway.bind`。",
                    ),
                ),
                row(
                    "port",
                    Kind::Int,
                    Def::Required,
                    t(
                        "TCP port. There is no fixed default: `twcore init` and `twcore remote enable` write a random port between 20000 and 32000 (never the gateway's) when they write this section. It cannot be 0 or the gateway's port.",
                        "TCP 端口。没有固定默认值：`twcore init` 和 `twcore remote enable` 写出这一节时随机写入 20000 到 32000 之间的一个端口（不会和网关相同）。不能是 0，也不能和网关端口相同。",
                    ),
                ),
                row(
                    "allow_from",
                    Kind::Strs,
                    Def::Is("[10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, fc00::/7]"),
                    t(
                        "Sources that may connect, as for `listen.gateway.allow_from`, except that this machine is not let in automatically (it has the local channel). A connection from anywhere else is closed before the handshake, without a byte in reply; narrowing the list also closes open connections it no longer allows. A source that fails the handshake 5 times within a minute is ignored for a minute.",
                        "允许连接的来源，写法同 `listen.gateway.allow_from`，但本机不会自动放行（本机有本地通道）。其他来源的连接在握手之前关闭，不回任何字节；收窄名单时，已经连着、不再放行的连接也随即断开。同一来源一分钟内握手失败 5 次，之后一分钟不理它。",
                    ),
                ),
            ],
        },
        // ── clients ───────────────────────────────────────────
        Section {
            path: "clients[]",
            ty: checked!(Client, "{name: a, key: k}"),
            rows: vec![
                row(
                    "name",
                    Kind::Str,
                    Def::Required,
                    t(
                        "Name of the key; unique. Routing rules match it with `when.client`.",
                        "密钥的名字，不能重复。路由规则用 `when.client` 匹配它。",
                    ),
                ),
                row(
                    "key",
                    Kind::Str,
                    Def::Required,
                    t(
                        "The key clients send (as `x-api-key` or `Authorization: Bearer`). Generated keys start with `tw-` so they are not mistaken for an upstream's key. Unique.",
                        "客户端发送的密钥（放在 `x-api-key` 或 `Authorization: Bearer` 中）。生成的密钥以 `tw-` 开头，以免被误认作上游的密钥。不能重复。",
                    ),
                ),
                row(
                    "max_concurrent",
                    Kind::Int,
                    Def::Unset,
                    t(
                        "Requests with this key that may run at once; the rest wait. Unset: no limit. `0` is refused.",
                        "用这把密钥同时进行的请求数上限，超出的排队等待。不写：不限。`0` 会被拒绝。",
                    ),
                ),
                row(
                    "allow",
                    Kind::Strs,
                    Def::Unset,
                    t(
                        "Models this key may use, as model ids or globs (`claude-*`). Unset: every model. `[]`: none at all.",
                        "这把密钥可用的模型，写模型 ID 或通配（`claude-*`）。不写：全部模型。`[]`：一个都不给。",
                    ),
                ),
                row(
                    "route",
                    Kind::Str,
                    Def::Unset,
                    t(
                        "Name of the route requests with this key take. Unset: `default_route`.",
                        "这把密钥的请求走哪条路由。不写：`default_route`。",
                    ),
                ),
                row(
                    "client",
                    Kind::Str,
                    Def::Unset,
                    t(
                        "The client this key was made for (`claude-code`, `codex`, …), recorded when the desktop app points a client at the gateway. A client has at most one.",
                        "这把密钥是为哪个客户端生成的（`claude-code`、`codex` 等），由桌面应用接管客户端时写入。一个客户端最多一把。",
                    ),
                ),
                row(
                    "disabled",
                    Kind::Bool,
                    Def::Is("false"),
                    t(
                        "Refuse every request made with this key, and keep the key.",
                        "拒绝使用这把密钥的所有请求，密钥本身保留。",
                    ),
                ),
            ],
        },
        // ── providers ─────────────────────────────────────────
        Section {
            path: "providers[]",
            ty: checked!(Provider, "{name: p, base_url: 'https://api.example.com'}"),
            rows: vec![
                row(
                    "name",
                    Kind::Str,
                    Def::Required,
                    t(
                        "Name of the upstream; unique, and not the name of a group. Names starting with `__` are reserved.",
                        "上游的名字，不能重复，也不能和策略组同名。以 `__` 开头的名字保留给内置项。",
                    ),
                ),
                row(
                    "base_url",
                    Kind::Str,
                    Def::Required,
                    t(
                        "Endpoint, `http://` or `https://`, up to the version segment where the provider documents one (`https://api.anthropic.com`, `https://api.openai.com/v1`).",
                        "接口地址，`http://` 或 `https://`，按服务商文档写到版本段为止（`https://api.anthropic.com`、`https://api.openai.com/v1`）。",
                    ),
                ),
                row(
                    "key",
                    Kind::Secret,
                    Def::Unset,
                    t(
                        "API key. It goes in the header the protocol expects: `x-api-key` (Anthropic), `Authorization: Bearer` (OpenAI), `x-goog-api-key` (Gemini). Leave it out for upstreams without a key, or when the credential is written in `headers`. Cannot be combined with `oauth`.",
                        "API 密钥，放进协议规定的请求头：`x-api-key`（Anthropic）、`Authorization: Bearer`（OpenAI）、`x-goog-api-key`（Gemini）。上游不需要密钥、或凭据写在 `headers` 里时不写。不能和 `oauth` 同时写。",
                    ),
                ),
                row(
                    "headers",
                    Kind::Headers,
                    Def::Is("{}"),
                    t(
                        "Additional request headers, in the order written; values may use `${VAR}`, and `{{access_token}}` where `oauth` is set. At most 32. Headers HTTP or the gateway manages (`host`, `content-length`, `connection`, …) cannot be set.",
                        "额外的请求头，按书写顺序发送；值可以用 `${VAR}`，配置了 `oauth` 时可以用 `{{access_token}}`。最多 32 个。HTTP 或网关管理的请求头（`host`、`content-length`、`connection` 等）不能设置。",
                    ),
                ),
                row(
                    "oauth",
                    Kind::Obj("providers[].oauth"),
                    Def::Unset,
                    t(
                        "OAuth credential: an access token obtained from a refresh token. Instead of `key`.",
                        "OAuth 凭据：用 refresh token 换取 access token。与 `key` 二选一。",
                    ),
                ),
                row(
                    "protocol",
                    Kind::Enum(protocols),
                    Def::Unset,
                    t(
                        "API format of the upstream. Unset: recognized from `base_url` for the official endpoints, otherwise treated as `anthropic`.",
                        "上游的接口格式。不写：官方地址按 `base_url` 识别，其余按 `anthropic` 处理。",
                    ),
                ),
                row(
                    "proxy",
                    Kind::Str,
                    Def::Is("direct"),
                    t(
                        "`direct`; `system`, the proxy in the core process's `HTTPS_PROXY`, `HTTP_PROXY` or `ALL_PROXY` environment variables; or the name of an entry in `proxies`.",
                        "`direct`；`system`，即 core 进程环境变量 `HTTPS_PROXY`、`HTTP_PROXY`、`ALL_PROXY` 中的代理；或 `proxies` 中某一项的名字。",
                    ),
                ),
                row(
                    "on_proxy_fail",
                    Kind::Enum(on_proxy_fail),
                    Def::Is("fail"),
                    t(
                        "When the proxy cannot be reached: `fail` the request, or go `direct`.",
                        "代理不可用时：请求失败（`fail`），或改为直连（`direct`）。",
                    ),
                ),
                row(
                    "models",
                    Kind::Strs,
                    Def::Is("[]"),
                    t(
                        "Models to assume when the upstream does not answer `/v1/models`.",
                        "上游不支持 `/v1/models` 时，按这份清单认定它提供的模型。",
                    ),
                ),
                row(
                    "models_only",
                    Kind::Strs,
                    Def::Unset,
                    t(
                        "Use only these of the upstream's models, as ids or globs. Others are not listed and are not routed here. Unset: all of them. Empty is refused; use `disabled`.",
                        "只使用这家的这些模型，写 ID 或通配。范围外的模型不出现在模型列表里，也不会路由到这家。不写：全部。写空列表会被拒绝，暂停使用请用 `disabled`。",
                    ),
                ),
                row(
                    "billing",
                    Kind::Enum(billings),
                    Def::Is("per-token"),
                    t(
                        "`per-token`: cost is usage times the price in the upstream's price sheet, subscription accounts included. `free`: cost is recorded as 0.",
                        "`per-token`：费用为用量乘以所选价目表中的单价，订阅账号同样如此。`free`：费用记为 0。",
                    ),
                ),
                row(
                    "pricing",
                    Kind::Str,
                    Def::Unset,
                    t(
                        "Name of a price sheet under `pricing.sheets`. Unset: the default price table.",
                        "`pricing.sheets` 中某张价目表的名字。不写：默认价目表。",
                    ),
                ),
                row(
                    "disabled",
                    Kind::Bool,
                    Def::Is("false"),
                    t(
                        "Take the upstream out of routing and out of the model list, and keep its configuration.",
                        "不参与路由，模型也不出现在模型列表里；配置原样保留。",
                    ),
                ),
            ],
        },
        Section {
            path: "providers[].oauth",
            ty: checked!(
                OAuth,
                "{refresh: r, endpoint: 'https://auth.example.com/token'}"
            ),
            rows: vec![
                row(
                    "access",
                    Kind::Str,
                    Def::Unset,
                    t(
                        "Current access token. Written back by the gateway after every refresh; unset means one is obtained on first use.",
                        "当前的 access token。每次刷新后由网关写回；不写则在第一次使用时换取。",
                    ),
                ),
                row(
                    "expires_at",
                    Kind::Str,
                    Def::Unset,
                    t(
                        "When `access` expires, RFC 3339 in UTC. Written back with it. Unset: used until the upstream answers 401.",
                        "`access` 的过期时间，RFC 3339（UTC），随 token 一起写回。不写：一直用到上游返回 401。",
                    ),
                ),
                row(
                    "refresh",
                    Kind::Str,
                    Def::Required,
                    t(
                        "Refresh token. When the token endpoint issues a new one, the old one stops working, so the gateway writes the new one back into this file.",
                        "Refresh token。token 端点换发新的之后旧的即作废，因此网关会把新的写回本文件。",
                    ),
                ),
                row(
                    "endpoint",
                    Kind::Str,
                    Def::Required,
                    t("Token endpoint URL.", "token 端点的地址。"),
                ),
                row(
                    "client_id",
                    Kind::Str,
                    Def::Unset,
                    t(
                        "OAuth client id, if the endpoint wants one.",
                        "OAuth 客户端 ID，端点需要时填写。",
                    ),
                ),
                row(
                    "client_secret",
                    Kind::Str,
                    Def::Unset,
                    t(
                        "OAuth client secret, if the endpoint wants one.",
                        "OAuth 客户端密钥，端点需要时填写。",
                    ),
                ),
                row(
                    "refresh_before",
                    Kind::Duration,
                    Def::Unset,
                    t(
                        "How long before expiry to refresh. Unset or unreadable: `5m`.",
                        "提前多久刷新。不写或写法无法识别：`5m`。",
                    ),
                ),
            ],
        },
        // ── proxies ───────────────────────────────────────────
        Section {
            path: "proxies[]",
            ty: checked!(Proxy, "{name: p, addr: '127.0.0.1:7890'}"),
            rows: vec![
                row(
                    "name",
                    Kind::Str,
                    Def::Required,
                    t(
                        "Name used in `providers[].proxy`. `direct` and `system` are built in.",
                        "`providers[].proxy` 引用的名字。`direct` 和 `system` 是内置的。",
                    ),
                ),
                row(
                    "type",
                    Kind::Enum(proxy_kinds),
                    Def::Is("socks5h"),
                    t(
                        "`socks5h` sends the host name to the proxy to resolve; `socks5` resolves it locally first. `http` and `https` are HTTP proxies.",
                        "`socks5h` 把域名交给代理解析；`socks5` 先在本地解析。`http` 和 `https` 是 HTTP 代理。",
                    ),
                ),
                row(
                    "addr",
                    Kind::Str,
                    Def::Required,
                    t("`host:port` of the proxy.", "代理的 `host:port`。"),
                ),
                row(
                    "auth",
                    Kind::Obj("proxies[].auth"),
                    Def::Unset,
                    t(
                        "User name and password, if the proxy wants them.",
                        "代理需要时填写用户名和密码。",
                    ),
                ),
            ],
        },
        Section {
            path: "proxies[].auth",
            ty: checked!(ProxyAuth, "{user: u, pass: p}"),
            rows: vec![
                row(
                    "user",
                    Kind::Str,
                    Def::Required,
                    t("User name.", "用户名。"),
                ),
                row(
                    "pass",
                    Kind::Secret,
                    Def::Required,
                    t("Password.", "密码。"),
                ),
            ],
        },
        // ── pricing ───────────────────────────────────────────
        Section {
            path: "pricing",
            ty: checked!(PricingConfig, "{}"),
            rows: vec![
                row(
                    "auto_update",
                    Kind::Bool,
                    Def::Is("true"),
                    t(
                        "Refresh the default price table from the network once a day. It is saved as `model_prices.json` beside `config.yaml`; the table built into the binary is used until then and when offline.",
                        "每天联网刷新一次默认价目表，保存为 `config.yaml` 旁边的 `model_prices.json`；此前以及离线时使用内置于程序中的价目表。",
                    ),
                ),
                row(
                    "sheets",
                    Kind::Objs("pricing.sheets[]"),
                    Def::Is("[]"),
                    t(
                        "Price sheets of your own. An upstream uses one with `providers[].pricing`.",
                        "自定义价目表。上游用 `providers[].pricing` 选用。",
                    ),
                ),
            ],
        },
        Section {
            path: "pricing.sheets[]",
            ty: checked!(SheetDef, "{name: s}"),
            rows: vec![
                row(
                    "name",
                    Kind::Str,
                    Def::Required,
                    t("Name of the sheet; unique.", "价目表的名字，不能重复。"),
                ),
                row(
                    "multiplier",
                    Kind::Num,
                    Def::Is("1"),
                    t(
                        "Applied to every price of the default table, cache and long-context prices included.",
                        "作用于默认价目表的全部单价，包括缓存和长上下文单价。",
                    ),
                ),
                row(
                    "models",
                    Kind::ObjMap(t("model id", "模型 ID"), "pricing.sheets[].models.*"),
                    Def::Is("{}"),
                    t(
                        "Prices for single models. They replace the default table's price for that model and are not multiplied.",
                        "单独定价的模型。它们取代默认价目表中该模型的单价，不乘倍率。",
                    ),
                ),
            ],
        },
        Section {
            path: "pricing.sheets[].models.*",
            ty: checked!(
                PerMillion,
                "{input: 3, output: 15, cache_read: 0.3, cache_write_5m: 3.75, cache_write_1h: 6}"
            ),
            rows: vec![
                row(
                    "input",
                    Kind::Num,
                    Def::Required,
                    t(
                        "US dollars per million input tokens.",
                        "每百万输入 token 的美元价格。",
                    ),
                ),
                row(
                    "output",
                    Kind::Num,
                    Def::Required,
                    t(
                        "US dollars per million output tokens.",
                        "每百万输出 token 的美元价格。",
                    ),
                ),
                row(
                    "cache_read",
                    Kind::Num,
                    Def::Required,
                    t(
                        "US dollars per million tokens read from the prompt cache.",
                        "每百万缓存读取 token 的美元价格。",
                    ),
                ),
                row(
                    "cache_write_5m",
                    Kind::Num,
                    Def::Required,
                    t(
                        "US dollars per million tokens written to a 5-minute cache.",
                        "每百万写入 5 分钟缓存 token 的美元价格。",
                    ),
                ),
                row(
                    "cache_write_1h",
                    Kind::Num,
                    Def::Required,
                    t(
                        "US dollars per million tokens written to a 1-hour cache.",
                        "每百万写入 1 小时缓存 token 的美元价格。",
                    ),
                ),
                row(
                    "input_above_200k",
                    Kind::Num,
                    Def::Unset,
                    t(
                        "Input price once a request's input exceeds 200K tokens. Written together with `output_above_200k`, or neither.",
                        "单次请求输入超过 200K token 后的输入单价。与 `output_above_200k` 同时写或都不写。",
                    ),
                ),
                row(
                    "output_above_200k",
                    Kind::Num,
                    Def::Unset,
                    t(
                        "Output price once a request's input exceeds 200K tokens.",
                        "单次请求输入超过 200K token 后的输出单价。",
                    ),
                ),
            ],
        },
        // ── client_probes ─────────────────────────────────────
        Section {
            path: "client_probes",
            ty: checked!(ClientProbes, "{}"),
            rows: vec![
                row(
                    "health_check",
                    Kind::Enum(probe_actions),
                    Def::Is("intercept"),
                    t(
                        "Connectivity checks (`max_tokens: 1`). Answered locally by default: nothing is lost.",
                        "连通性检查（`max_tokens: 1`）。默认在本地应答，不影响任何功能。",
                    ),
                ),
                row(
                    "warmup",
                    Kind::Enum(probe_actions),
                    Def::Is("intercept"),
                    t(
                        "Warm-up requests. Answered locally by default.",
                        "预热请求。默认在本地应答。",
                    ),
                ),
                row(
                    "titling",
                    Kind::Enum(probe_actions),
                    Def::Is("passthrough"),
                    t(
                        "Requests that name a session. Passed through by default: intercepting them gives every session the same title.",
                        "为会话起标题的请求。默认放行：拦下后所有会话都会是同一个标题。",
                    ),
                ),
                row(
                    "topic_detect",
                    Kind::Enum(probe_actions),
                    Def::Is("passthrough"),
                    t(
                        "Topic detection. Passed through by default.",
                        "话题检测。默认放行。",
                    ),
                ),
                row(
                    "suggestion",
                    Kind::Enum(probe_actions),
                    Def::Is("passthrough"),
                    t(
                        "Suggestions. Passed through by default.",
                        "建议。默认放行。",
                    ),
                ),
            ],
        },
        // ── security ──────────────────────────────────────────
        Section {
            path: "security",
            ty: checked!(Security, "{}"),
            rows: vec![
                row(
                    "redact",
                    Kind::Obj("security.redact"),
                    Def::Section,
                    t(
                        "Outbound redaction: credentials found in a request are replaced before it leaves.",
                        "出站脱敏：请求发出前，把其中的凭据替换掉。",
                    ),
                ),
                row(
                    "inspect_tools",
                    Kind::Obj("security.inspect_tools"),
                    Def::Section,
                    t(
                        "Tool-call inspection: dangerous commands in the tool calls a model returns cut the response off.",
                        "工具调用审查：模型返回的工具调用中出现危险命令时切断响应。",
                    ),
                ),
                row(
                    "hidden_text",
                    Kind::Obj("security.hidden_text"),
                    Def::Section,
                    t(
                        "Hidden characters that people cannot see and models can read refuse the request.",
                        "人看不见、模型读得到的隐藏字符，出现时拒绝请求。",
                    ),
                ),
                row(
                    "content",
                    Kind::Obj("security.content"),
                    Def::Section,
                    t(
                        "Content filter: words or patterns in what the caller sends refuse the request.",
                        "内容过滤：调用方发送的内容中出现指定的词或写法时拒绝请求。",
                    ),
                ),
                row(
                    "output_limit",
                    Kind::Obj("security.output_limit"),
                    Def::Section,
                    t(
                        "Output length: a response longer than the limit is cut off.",
                        "输出长度：回答超过上限时切断。",
                    ),
                ),
            ],
        },
        Section {
            path: "security.redact",
            ty: checked!(RedactPolicy, "{}"),
            rows: vec![
                row("mode", Kind::Enum(modes), Def::Is("observe"), MODE_DOC),
                row("enable", Kind::Strs, Def::Is("[]"), ENABLE_DOC),
                row("disable", Kind::Strs, Def::Is("[]"), DISABLE_DOC),
                row(
                    "custom",
                    Kind::Objs("security.redact.custom[]"),
                    Def::Is("[]"),
                    t(
                        "Rules of your own: whatever a pattern matches is treated as a credential.",
                        "自定义规则：正则匹配到的内容按凭据处理。",
                    ),
                ),
            ],
        },
        Section {
            path: "security.redact.custom[]",
            ty: checked!(CustomRedactRule, "{name: n, pattern: p}"),
            rows: vec![
                row("name", Kind::Str, Def::Required, RULE_NAME),
                row(
                    "pattern",
                    Kind::Str,
                    Def::Required,
                    t("Regular expression.", "正则表达式。"),
                ),
                row("disabled", Kind::Bool, Def::Is("false"), RULE_DISABLED),
            ],
        },
        Section {
            path: "security.inspect_tools",
            ty: checked!(ToolPolicy, "{}"),
            rows: vec![
                row("mode", Kind::Enum(modes), Def::Is("observe"), MODE_DOC),
                row("enable", Kind::Strs, Def::Is("[]"), ENABLE_DOC),
                row("disable", Kind::Strs, Def::Is("[]"), DISABLE_DOC),
                row(
                    "actions",
                    Kind::EnumMap(RULE_ID, tool_actions),
                    Def::Is("{}"),
                    t(
                        "What a built-in rule does under `enforce`, written only where it differs from the factory setting (`rm-rf-root: record`).",
                        "内置规则在 `enforce` 下的处置，只写与出厂不同的（`rm-rf-root: record`）。",
                    ),
                ),
                row(
                    "custom",
                    Kind::Objs("security.inspect_tools.custom[]"),
                    Def::Is("[]"),
                    t(
                        "Rules of your own, matched against the arguments of a tool call.",
                        "自定义规则，按工具调用的参数匹配。",
                    ),
                ),
            ],
        },
        Section {
            path: "security.inspect_tools.custom[]",
            ty: checked!(CustomToolRule, "{name: n, pattern: p}"),
            rows: vec![
                row("name", Kind::Str, Def::Required, RULE_NAME),
                row(
                    "pattern",
                    Kind::Str,
                    Def::Required,
                    t("Regular expression.", "正则表达式。"),
                ),
                row(
                    "action",
                    Kind::Enum(tool_actions),
                    Def::Is("record"),
                    t(
                        "Under `enforce`: `cut` the response off, or only `record` the match.",
                        "`enforce` 下切断响应（`cut`），或只记录（`record`）。",
                    ),
                ),
                row("disabled", Kind::Bool, Def::Is("false"), RULE_DISABLED),
            ],
        },
        Section {
            path: "security.hidden_text",
            ty: checked!(HiddenPolicy, "{}"),
            rows: vec![
                row("mode", Kind::Enum(modes), Def::Is("observe"), MODE_DOC),
                row(
                    "disable",
                    Kind::Strs,
                    Def::Is("[]"),
                    t(
                        "Kinds not to look for: `tag`, `bidi`.",
                        "不检查的种类：`tag`、`bidi`。",
                    ),
                ),
            ],
        },
        Section {
            path: "security.content",
            ty: checked!(ContentPolicy, "{}"),
            rows: vec![
                row("mode", Kind::Enum(modes), Def::Is("observe"), MODE_DOC),
                row("enable", Kind::Strs, Def::Is("[]"), ENABLE_DOC),
                row("disable", Kind::Strs, Def::Is("[]"), DISABLE_DOC),
                row(
                    "actions",
                    Kind::EnumMap(RULE_ID, content_actions),
                    Def::Is("{}"),
                    t(
                        "What a built-in rule does under `enforce`, written only where it differs from the factory setting.",
                        "内置规则在 `enforce` 下的处置，只写与出厂不同的。",
                    ),
                ),
                row(
                    "custom",
                    Kind::Objs("security.content.custom[]"),
                    Def::Is("[]"),
                    t("Rules of your own.", "自定义规则。"),
                ),
            ],
        },
        Section {
            path: "security.content.custom[]",
            ty: checked!(CustomContentRule, "{name: n, pattern: p}"),
            rows: vec![
                row("name", Kind::Str, Def::Required, RULE_NAME),
                row(
                    "pattern",
                    Kind::Str,
                    Def::Required,
                    t(
                        "A keyword, or a regular expression with `match: regex`. Case-insensitive either way.",
                        "关键词；`match: regex` 时为正则表达式。均不区分大小写。",
                    ),
                ),
                row(
                    "match",
                    Kind::Enum(content_matches),
                    Def::Is("contains"),
                    t(
                        "`contains`: the text contains `pattern`. `regex`: `pattern` is a regular expression.",
                        "`contains`：正文包含 `pattern`。`regex`：`pattern` 是正则表达式。",
                    ),
                ),
                row(
                    "action",
                    Kind::Enum(content_actions),
                    Def::Is("record"),
                    t(
                        "Under `enforce`: `block` the request, or only `record` the match.",
                        "`enforce` 下拒绝请求（`block`），或只记录（`record`）。",
                    ),
                ),
                row("disabled", Kind::Bool, Def::Is("false"), RULE_DISABLED),
            ],
        },
        Section {
            path: "security.output_limit",
            ty: checked!(OutputLimitPolicy, "{}"),
            rows: vec![
                row(
                    "mode",
                    Kind::Enum(modes),
                    Def::Is("off"),
                    t(
                        "Off out of the box: no single limit suits every use. `observe` records long responses; `enforce` stops the stream at the limit.",
                        "出厂关闭：没有一个上限适合所有用途。`observe` 记录超长的回答；`enforce` 在超过上限处停止输出。",
                    ),
                ),
                row(
                    "max_chars",
                    Kind::Int,
                    Def::Is("100000"),
                    t(
                        "Limit in characters (Unicode scalar values), from 1 to 1000000.",
                        "上限，按字符（Unicode 标量）计，取值 1 到 1000000。",
                    ),
                ),
            ],
        },
        // ── retention ─────────────────────────────────────────
        Section {
            path: "retention",
            ty: checked!(Retention, "{}"),
            rows: vec![
                row(
                    "body_days",
                    Kind::Int,
                    Def::Is("7"),
                    t(
                        "Days to keep request and response bodies.",
                        "请求和响应正文保留的天数。",
                    ),
                ),
                row(
                    "row_days",
                    Kind::Int,
                    Def::Is("90"),
                    t(
                        "Days to keep the record of each request (time, model, usage, cost).",
                        "每条请求记录（时间、模型、用量、费用）保留的天数。",
                    ),
                ),
                row(
                    "body_max_bytes",
                    Kind::Int,
                    Def::Is("2147483648"),
                    t(
                        "Upper bound on the bytes bodies may take; beyond it the oldest days go first. The default is 2 GiB.",
                        "正文最多占用的字节数，超出时从最早的日期开始删除。默认 2 GiB。",
                    ),
                ),
            ],
        },
        // ── groups / routes ───────────────────────────────────
        Section {
            path: "groups[]",
            ty: checked!(Group, "{name: g, providers: [a]}"),
            rows: vec![
                row(
                    "name",
                    Kind::Str,
                    Def::Required,
                    t(
                        "Name of the group; unique, and not the name of an upstream.",
                        "策略组的名字，不能重复，也不能和上游同名。",
                    ),
                ),
                row(
                    "type",
                    Kind::Enum(group_types),
                    Def::Is("fallback"),
                    t(
                        "`fallback`: the first healthy member, in order. `select`: the member named in `selected`. `load-balance`: take turns. `url-test`: the fastest by measured time to first byte. `cheapest`: the lowest input price.",
                        "`fallback`：按顺序取第一个健康的。`select`：取 `selected` 指定的那个。`load-balance`：轮流。`url-test`：按实测首字节时间取最快的。`cheapest`：取输入单价最低的。",
                    ),
                ),
                row(
                    "providers",
                    Kind::Strs,
                    Def::Required,
                    t("Member upstreams, by name.", "成员上游的名字。"),
                ),
                row(
                    "session_affinity",
                    Kind::Bool,
                    Def::Is("true"),
                    t(
                        "Keep a session on the same upstream so its prompt cache keeps hitting. Turning it off under `load-balance` spreads every turn and loses the cache.",
                        "同一会话固定走同一家，使 prompt cache 持续命中。在 `load-balance` 下关闭会让每一轮都换一家，缓存随之失效。",
                    ),
                ),
                row(
                    "selected",
                    Kind::Str,
                    Def::Unset,
                    t(
                        "For `select`: the chosen member.",
                        "`select` 类型选中的成员。",
                    ),
                ),
            ],
        },
        Section {
            path: "routes[]",
            ty: checked!(RouteSet, "{name: r}"),
            rows: vec![
                row(
                    "name",
                    Kind::Str,
                    Def::Required,
                    t(
                        "Name of the route; unique. `default` is the one keys use unless told otherwise.",
                        "路由的名字，不能重复。`default` 是密钥默认使用的那条。",
                    ),
                ),
                row(
                    "rules",
                    Kind::Objs("routes[].rules[]"),
                    Def::Is("[]"),
                    t(
                        "Evaluated top to bottom; the first rule with `to` or `deny` that matches decides where the request goes.",
                        "自上而下求值；第一条匹配且带有 `to` 或 `deny` 的规则决定请求去向。",
                    ),
                ),
            ],
        },
        Section {
            path: "routes[].rules[]",
            ty: checked!(Rule, "{name: r}"),
            rows: vec![
                row(
                    "name",
                    Kind::Str,
                    Def::Required,
                    t(
                        "Name shown in logs and in the traffic view.",
                        "日志和流量详情中显示的名字。",
                    ),
                ),
                row(
                    "when",
                    Kind::Obj("routes[].rules[].when"),
                    Def::Unset,
                    t(
                        "Conditions, all of which have to hold. Unset: matches every request.",
                        "条件，须全部满足。不写：匹配所有请求。",
                    ),
                ),
                row(
                    "to",
                    Kind::Str,
                    Def::Unset,
                    t(
                        "An upstream or a group, by name; `__all__` is every upstream in declared order. Not allowed together with `when.provider_would_be`.",
                        "上游或策略组的名字；`__all__` 表示按声明顺序的全部上游。不能与 `when.provider_would_be` 同时写。",
                    ),
                ),
                row(
                    "set",
                    Kind::Obj("routes[].rules[].set"),
                    Def::Unset,
                    t(
                        "Parameters to rewrite. Collected from every matching rule, not only the first.",
                        "改写请求参数。从所有匹配的规则累积，不只第一条。",
                    ),
                ),
                row(
                    "deny",
                    Kind::Str,
                    Def::Unset,
                    t(
                        "Refuse the request with this reason.",
                        "以这句原因拒绝请求。",
                    ),
                ),
            ],
        },
        Section {
            path: "routes[].rules[].when",
            ty: checked!(When, "{}"),
            rows: vec![
                row(
                    "model",
                    Kind::Str,
                    Def::Unset,
                    t(
                        "Requested model, glob (`claude-opus-*`).",
                        "请求的模型，可用通配（`claude-opus-*`）。",
                    ),
                ),
                row(
                    "client",
                    Kind::Str,
                    Def::Unset,
                    t(
                        "Name of the gateway key the request used, exactly.",
                        "请求所用网关密钥的名字，精确匹配。",
                    ),
                ),
                row(
                    "dialect",
                    Kind::Str,
                    Def::Unset,
                    t(
                        "API format the client spoke: `anthropic`, `openai-chat`, `openai-responses`, `gemini`.",
                        "客户端使用的接口格式：`anthropic`、`openai-chat`、`openai-responses`、`gemini`。",
                    ),
                ),
                row(
                    "input_tokens",
                    Kind::Compare,
                    Def::Unset,
                    t("Estimated input tokens.", "估算的输入 token 数。"),
                ),
                row(
                    "max_tokens",
                    Kind::Compare,
                    Def::Unset,
                    t(
                        "The request's `max_tokens`. A request without one never matches.",
                        "请求中的 `max_tokens`。未写该参数的请求不匹配。",
                    ),
                ),
                row(
                    "tool_count",
                    Kind::Compare,
                    Def::Unset,
                    t("Number of tools offered.", "请求中提供的工具数量。"),
                ),
                row(
                    "cache",
                    Kind::Bool,
                    Def::Unset,
                    t(
                        "Whether the request uses the prompt cache.",
                        "请求是否使用 prompt cache。",
                    ),
                ),
                row(
                    "tools",
                    Kind::Bool,
                    Def::Unset,
                    t("Whether the request offers tools.", "请求是否带工具。"),
                ),
                row(
                    "image",
                    Kind::Bool,
                    Def::Unset,
                    t(
                        "Whether the request contains an image.",
                        "请求是否包含图片。",
                    ),
                ),
                row(
                    "thinking",
                    Kind::Bool,
                    Def::Unset,
                    t("Whether extended thinking is on.", "是否开启扩展思考。"),
                ),
                row(
                    "stream",
                    Kind::Bool,
                    Def::Unset,
                    t("Whether the response is streamed.", "是否流式返回。"),
                ),
                row(
                    "intent",
                    Kind::OneOrMany,
                    Def::Unset,
                    t(
                        "A client helper request: `assistant_internal` for any of them, or one class (`titling`). Only classes set to `route` in `client_probes` reach routing.",
                        "客户端的辅助请求：`assistant_internal` 表示任意一类，也可以写具体的一类（`titling`）。只有在 `client_probes` 中设为 `route` 的类别才会进入路由。",
                    ),
                ),
                row(
                    "provider_would_be",
                    Kind::OneOrMany,
                    Def::Unset,
                    t(
                        "The upstream routing chose. Such a rule is evaluated after routing, may only `set` or `deny`, and cannot have `to`.",
                        "路由选中的上游。这类规则在路由完成后求值，只能 `set` 或 `deny`，不能写 `to`。",
                    ),
                ),
            ],
        },
        Section {
            path: "routes[].rules[].set",
            ty: checked!(SetAction, "{}"),
            rows: vec![
                row(
                    "model",
                    Kind::Str,
                    Def::Unset,
                    t(
                        "Send a different model. The prompt cache of the session is lost.",
                        "换成另一个模型发送。该会话的 prompt cache 随之失效。",
                    ),
                ),
                row(
                    "max_tokens",
                    Kind::Int,
                    Def::Unset,
                    t("Replace `max_tokens`.", "替换 `max_tokens`。"),
                ),
                row(
                    "thinking",
                    Kind::Bool,
                    Def::Unset,
                    t("Turn extended thinking on or off.", "开启或关闭扩展思考。"),
                ),
                row(
                    "only_at_session_start",
                    Kind::Bool,
                    Def::Is("false"),
                    t(
                        "Apply only when a session starts. Recorded and shown; not in effect yet.",
                        "只在会话开始时应用。目前只记录和显示，尚未生效。",
                    ),
                ),
            ],
        },
    ]
}

/// 一份内置规则清单，渲染成表。
pub fn rules(kind: &str, l: Lang) -> Option<String> {
    let zh = l == Lang::Zh;
    let yes = if zh { "开" } else { "on" };
    let no = if zh { "关" } else { "off" };
    let mut out = String::new();
    match kind {
        "redact" => {
            out += if zh {
                "| id | 名称 | 出厂 |\n|---|---|---|\n"
            } else {
                "| id | Name | Out of the box |\n|---|---|---|\n"
            };
            for b in tw_guard::redact::rules::BUILTINS {
                let on = if b.on_by_default { yes } else { no };
                out += &format!("| `{}` | {} | {on} |\n", b.id, b.name);
            }
        }
        "inspect_tools" => {
            out += if zh {
                "| id | 名称 | `enforce` 下出厂处置 |\n|---|---|---|\n"
            } else {
                "| id | Name | Under `enforce`, out of the box |\n|---|---|---|\n"
            };
            for s in &tw_guard::tools::rules::builtin().dangerous {
                let a = ToolAction::factory(s).slug();
                out += &format!("| `{}` | {} | `{a}` |\n", s.id, s.name);
            }
        }
        "content" => {
            out += if zh {
                "| id | 名称 | 分组 | 出厂 | `enforce` 下出厂处置 |\n|---|---|---|---|---|\n"
            } else {
                "| id | Name | Group | Out of the box | Under `enforce`, out of the box |\n|---|---|---|---|---|\n"
            };
            for b in tw_guard::content::builtins() {
                let on = if b.on_by_default { yes } else { no };
                let a = ContentAction::factory(b).slug();
                out += &format!("| `{}` | {} | {} | {on} | `{a}` |\n", b.id, b.name, b.group);
            }
        }
        "hidden_text" => {
            out += if zh {
                "| 种类 | 说明 |\n|---|---|\n"
            } else {
                "| Kind | What it is |\n|---|---|\n"
            };
            for k in tw_guard::hidden::SMUGGLING {
                let what = match (k.slug(), zh) {
                    ("tag", false) => {
                        "Unicode tag characters (U+E0000 to U+E007F): invisible everywhere, read by the model, able to carry a whole instruction."
                    }
                    ("tag", true) => {
                        "Unicode 标签字符（U+E0000 至 U+E007F）：在任何地方都不可见，模型却能读到，足以藏下一整段指令。"
                    }
                    ("bidi", false) => {
                        "Bidirectional control characters: make the order shown differ from the order the model reads."
                    }
                    ("bidi", true) => "双向控制符：使显示顺序与模型读到的顺序不一致。",
                    (other, _) => panic!("hidden kind `{other}` has no description in the manual"),
                };
                out += &format!("| `{}` | {what} |\n", k.slug());
            }
        }
        _ => return None,
    }
    Some(out)
}
