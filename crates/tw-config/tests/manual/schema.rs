//! config.yaml 的每一节、每一个字段，手册里那张表就是照这里渲染的。
//!
//! **改了 tw-config（或者它引用的 tw-engine、tw-pricing）的配置类型，就改这里**：
//! 加一行、删一行、改默认值。`manual.rs` 会拿这份声明和代码逐项对，对不上时
//! 说清楚是哪个字段、该怎么改。改完用 `UPDATE_CONFIG_DOCS=1` 重新生成手册。
//!
//! 说明写给手写配置文件的人：这个字段管什么、不写是什么意思、写错了会怎样。
//! 两种语言各写一遍，**不是互译的字面对照**，各自按各自的习惯说。

use super::{Def, Kind, Lang, Row, Section, T2, Ty};
use tw_config::proxy::ProxyAuth;
use tw_config::*;
use tw_engine::rule::When;
use tw_engine::{
    BalanceBy, Group, GroupType, Member, Pinned, RouteSet, Rule, SetAction, SetReasoning,
};
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
fn balance_settings() -> Vec<&'static str> {
    super::fields::<BalanceSetting>()
}
fn signed_in() -> Vec<&'static str> {
    super::fields::<SignedIn>()
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
fn redact_actions() -> Vec<&'static str> {
    super::fields::<RedactAction>()
}
fn content_scopes() -> Vec<&'static str> {
    super::fields::<ContentScope>()
}
fn content_matches() -> Vec<&'static str> {
    super::fields::<ContentMatch>()
}
/// 出站脱敏认的那几种：码位只有内容过滤认
fn redact_matches() -> Vec<&'static str> {
    super::fields::<ContentMatch>()
        .into_iter()
        .filter(|m| {
            let m: ContentMatch = serde_yaml_ng::from_str(m).unwrap();
            tw_guard::policy::redact_match("x", m, false).is_some()
        })
        .collect()
}
fn group_types() -> Vec<&'static str> {
    super::fields::<GroupType>()
}
fn balance_bys() -> Vec<&'static str> {
    super::fields::<BalanceBy>()
}
fn limit_pers() -> Vec<&'static str> {
    super::fields::<LimitPer>()
}
fn set_reasonings() -> Vec<&'static str> {
    super::fields::<SetReasoning>()
}

/// 同步位置三种写法各自的字段。`target` 是按 `type` 分的枚举，问 serde 要字段的办法问不出
/// 枚举每一种的字段，所以借它的报错：写一个不存在的字段，serde 说出这一种认得的全部字段
fn target_fields(kind: &str) -> Vec<&'static str> {
    let e = serde_yaml_ng::from_str::<tw_config::sync::Target>(&format!(
        "type: {kind}\n__probe__: 1\n"
    ))
    .expect_err("an unknown field is refused")
    .to_string();
    let expected = e
        .split("expected")
        .nth(1)
        .unwrap_or_else(|| panic!("serde did not list the fields: {e}"));
    let mut out: Vec<&'static str> = vec!["type"];
    out.extend(
        expected
            .split('`')
            .skip(1)
            .step_by(2)
            .map(|f| &*Box::leak(f.to_string().into_boxed_str())),
    );
    out
}
fn folder_fields() -> Vec<&'static str> {
    target_fields("folder")
}
fn webdav_fields() -> Vec<&'static str> {
    target_fields("webdav")
}
fn s3_fields() -> Vec<&'static str> {
    target_fields("s3")
}
fn target_type() -> Vec<&'static str> {
    vec!["type"]
}
fn target_types() -> Vec<&'static str> {
    vec!["folder", "webdav", "s3"]
}

const PROXY_DOC: T2 = t(
    "Where requests to the sync location go out: `direct`, `system` (the system proxy), or the name of a proxy under `proxies`. An `http://` address on this machine or the local network has to be `direct`.",
    "访问同步位置的请求从哪里出去：`direct`（直连）、`system`（跟随系统代理），或者 `proxies` 里一个代理的名字。本机、内网的 `http://` 地址只能是 `direct`。",
);

const RULE_ID: T2 = t("built-in rule id", "内置规则 id");
const MODE_DOC: T2 = t(
    "`off` does nothing (Off in the app); `observe` detects and records only, and changes nothing \
     (Monitor); `enforce` detects and handles each match by its rule's action (On).",
    "`off` 不检测（应用里叫「关闭」）；`observe` 检测并记录，不改变任何行为（「监控」）；`enforce` \
     检测并按每条规则的动作处理（「开启」）。",
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
pub const RULE_LISTS: &[&str] = &["redact", "inspect_tools", "content"];

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
                        "The three guards. All of them start in `observe`, so out of the box nothing is changed or refused.",
                        "三项防护。出厂时都处在 `observe`，不改变、不拒绝任何请求。",
                    ),
                ),
                row(
                    "retention",
                    Kind::Obj("retention"),
                    Def::Section,
                    t("How long request logs are kept.", "请求日志保留多久。"),
                ),
                row(
                    "failover",
                    Kind::Obj("failover"),
                    Def::Section,
                    t(
                        "How long an upstream is set aside after it fails, and how long the start of a stream is awaited.",
                        "上游失败后停用多久，以及流式回答的开头最多等多久。",
                    ),
                ),
                row(
                    "aliases",
                    Kind::OneOrManyMap(t("alias", "别名")),
                    Def::Is("{}"),
                    t(
                        "Model aliases: one name for the same model across upstreams, mapped to the name each upstream uses, in order. A request for an alias goes to any upstream offering one of the listed names, under the first of them it offers. An alias cannot list another alias.",
                        "模型别名：同一个模型在各上游的名称合用一个名字，按顺序列出各上游的名称。请求别名时，提供其中任一名称的上游都能服务，发给它的是列表中它提供的第一个名称。别名不能列出别的别名。",
                    ),
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
                row(
                    "plugins",
                    Kind::Objs("plugins[]"),
                    Def::Is("[]"),
                    t(
                        "Script plugins, in the order they run. The app installs them; each one's code and settings are a file next to this one.",
                        "脚本插件，按运行的顺序。由应用安装，每个插件的代码和设置是本文件旁边的一个文件。",
                    ),
                ),
                row(
                    "sync",
                    Kind::Obj("sync"),
                    Def::Unset,
                    t(
                        "Multi-device sync. A local setting: configuration packs and other devices never carry it, and restoring a backup keeps the current one. Unset: sync is off.",
                        "多设备同步。属于本机设置：配置包和其他设备不会带走它，恢复备份时保留当前值。不写：不同步。",
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
                        "`loopback` is this machine only; `all` is every interface; an interface name (`en0`, `eth0`) is looked up again every few seconds and follows address changes, and while the interface is not there the gateway listens on 127.0.0.1 only and adds it once it appears; a fixed IP address stops working when the address changes. Binding one interface also listens on 127.0.0.1.",
                        "`loopback` 只有本机；`all` 所有网卡；网卡名（`en0`、`eth0`）每隔几秒重新解析，地址变了也能跟上，网卡暂时不在时先只监听 127.0.0.1，出现后再补上；写死的 IP 地址在地址变化后失效。绑定单张网卡时同时监听 127.0.0.1。",
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
                        "Models this key may use, as model ids or globs (`claude-*`). An upstream model name also allows the aliases that list it; an alias allows only the alias. Unset: every model. `[]`: none at all.",
                        "这把密钥可用的模型，写模型 ID 或通配（`claude-*`）。写上游模型名，列有它的别名一并可用；写别名只放行别名。不写：全部模型。`[]`：一个都不给。",
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
                row(
                    "limits",
                    Kind::Objs("clients[].limits[]"),
                    Def::Unset,
                    t(
                        "Usage limits: requests, tokens or cost per minute, hour, day, week or month. A request has to pass every one. Unset: no limit.",
                        "用量上限：每分钟、每小时、每天、每周或每月的请求数、token 数或费用。请求要通过每一条。不写：不限。",
                    ),
                ),
            ],
        },
        Section {
            path: "clients[].limits[]",
            ty: checked!(KeyLimit, "{per: day}"),
            rows: vec![
                row(
                    "per",
                    Kind::Enum(limit_pers),
                    Def::Required,
                    t(
                        "The period. `minute` and `hour` are rolling (the last 60 seconds, the last 60 minutes, or `every` of them); `day`, `week` and `month` start again at local midnight, on Monday and on the 1st.",
                        "按多长一段时间算。`minute`、`hour` 是滚动的（最近 60 秒、最近 60 分钟，或者 `every` 个这样的单位）；`day`、`week`、`month` 在本地时间的零点、周一零点、每月一号零点重新算。",
                    ),
                ),
                row(
                    "every",
                    Kind::Int,
                    Def::Unset,
                    t(
                        "How many minutes or hours the rolling window spans: `{per: hour, every: 5}` is the last 5 hours. From 1 to 60 for `minute`, from 1 to 24 for `hour`; `day`, `week` and `month` do not take it. Unset: 1.",
                        "滚动窗口有几分钟、几小时：`{per: hour, every: 5}` 是最近 5 小时。`minute` 取 1 到 60，`hour` 取 1 到 24；`day`、`week`、`month` 不能写。不写：1。",
                    ),
                ),
                row(
                    "requests",
                    Kind::Int,
                    Def::Unset,
                    t(
                        "At most this many requests. Token counts, answers the gateway gives itself and requests that never reach an upstream do not count.",
                        "最多这么多个请求。数 token 的请求、网关自己答的、没有发到上游的不算。",
                    ),
                ),
                row(
                    "tokens",
                    Kind::Int,
                    Def::Unset,
                    t(
                        "At most this many tokens: uncached input, cache writes and output.",
                        "最多这么多 token：未命中缓存的输入、写入缓存的和输出。",
                    ),
                ),
                row(
                    "cost",
                    Kind::Num,
                    Def::Unset,
                    t(
                        "At most this much, in US dollars, as recorded for each request; at least 0.01. Models without a price and upstreams with `billing: free` count as 0.",
                        "最多花这么多美元，按每个请求记下的费用算；至少 0.01。没有价格的模型、`billing: free` 的上游算 0。",
                    ),
                ),
                row(
                    "cache_reads",
                    Kind::Bool,
                    Def::Is("false"),
                    t(
                        "Count cache reads too. Only for a `tokens` limit.",
                        "把读取缓存的 token 也算进去。只有 `tokens` 上限能写。",
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
                        "Endpoint, `http://` or `https://`, up to the version segment where the provider documents one (`https://api.anthropic.com`, `https://api.openai.com/v1`). For Bedrock, the region's runtime endpoint: `https://bedrock-runtime.<region>.amazonaws.com`. No backslashes, and no user name or password before the host (credentials go in `key` or `headers`).",
                        "接口地址，`http://` 或 `https://`，按服务商文档写到版本段为止（`https://api.anthropic.com`、`https://api.openai.com/v1`）。Bedrock 写所在区域的推理地址：`https://bedrock-runtime.<区域>.amazonaws.com`。地址中不能有反斜杠，主机前不能写用户名或密码（凭据写在 `key` 或 `headers` 中）。",
                    ),
                ),
                row(
                    "key",
                    Kind::Secret,
                    Def::Unset,
                    t(
                        "API key. It goes in the header the protocol expects: `x-api-key` (Anthropic), `Authorization: Bearer` (OpenAI, and a Bedrock API key), `x-goog-api-key` (Gemini). Leave it out for upstreams without a key, or when the credential is written in `headers`. Cannot be combined with `oauth` or `aws`.",
                        "API 密钥，放进协议规定的请求头：`x-api-key`（Anthropic）、`Authorization: Bearer`（OpenAI，以及 Bedrock API Key）、`x-goog-api-key`（Gemini）。上游不需要密钥、或凭据写在 `headers` 里时不写。不能和 `oauth`、`aws` 同时写。",
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
                    "aws",
                    Kind::Obj("providers[].aws"),
                    Def::Unset,
                    t(
                        "AWS access keys of a Bedrock upstream, written there or read from an AWS profile: every request is signed with them (SigV4). Instead of `key`, which holds a Bedrock API key.",
                        "Bedrock 上游的 AWS 访问密钥，写在这里或者从 AWS 的 profile 读：每个请求用它们签名（SigV4）。与 `key`（Bedrock API Key）二选一。",
                    ),
                ),
                row(
                    "protocol",
                    Kind::Enum(protocols),
                    Def::Unset,
                    t(
                        "API format of the upstream. Unset: recognized from `base_url` for the official endpoints (a Bedrock runtime endpoint is `bedrock`), otherwise treated as `anthropic`.",
                        "上游的接口格式。不写：官方地址按 `base_url` 识别（Bedrock 的推理地址是 `bedrock`），其余按 `anthropic` 处理。",
                    ),
                ),
                row(
                    "forward_client_identity",
                    Kind::Bool,
                    Def::Is("false"),
                    t(
                        "Also send the client's own identity: its `User-Agent`, identity headers such as `x-app` and `originator`, and identity fields in the request body such as `metadata.user_id`. Values are the client's, never made up. Off: requests carry ThinkWatch's `User-Agent` and no client identity. For upstreams that admit only certain clients (Kimi For Coding, Bailian Coding Plan, relays restricted to official clients). Not available for `chatgpt`.",
                        "同时发送客户端自己的身份：它的 `User-Agent`、`x-app` 和 `originator` 等身份请求头，以及请求体中的身份字段（如 `metadata.user_id`）。发送的都是客户端的原值，不做伪造。关闭时请求使用 ThinkWatch 的 `User-Agent`，不带客户端身份。用于只接受特定客户端的上游（Kimi For Coding、百炼 Coding Plan、只允许官方客户端的中转站）。`chatgpt` 不可用。",
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
                        "Models added by hand, by exact id, for those the upstream serves but leaves out of its list. They count as offered together with the models the upstream lists, or are the whole list when it lists none: they appear in `/v1/models` and requests for them are routed here. `models_only` still applies. No wildcards, no duplicates, at most 256 characters each.",
                        "手动添加的模型，写确切的 ID：上游能服务、却没有列进清单的模型。它们和上游列出的模型一起算作这家提供的模型，上游不提供清单时就是全部：出现在 `/v1/models` 里，相应的请求也会路由到这家。`models_only` 照样适用。不支持通配，不能重复，每项最多 256 个字符。",
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
                    "model_specs",
                    Kind::ObjMap(t("model id", "模型 ID"), "providers[].model_specs.*"),
                    Def::Is("{}"),
                    t(
                        "Context window, output limit, reasoning and image input of single models of this upstream, written by hand, by exact model id. They take precedence over the price table: for models it does not know, or gets wrong.",
                        "手写这家上游某些模型的上下文窗口、输出上限、会不会推理、收不收图，按模型 ID 完全匹配。写了就优先于价目表，用于价目表里没有或写错的模型。",
                    ),
                ),
                row(
                    "max_concurrent",
                    Kind::Int,
                    Def::Unset,
                    t(
                        "Most requests sent to this upstream at the same time, from 1 to 1000. When it is full, a conversation that stays on it waits for a free slot and other requests go to the next upstream; see `failover.wait_secs`. Unset: no limit.",
                        "同时发给这家的请求最多几个，取值 1 到 1000。满了的时候，留在这家的对话等空位，别的请求换下一家；等多久见 `failover.wait_secs`。不写：不限。",
                    ),
                ),
                row(
                    "min_speed",
                    Kind::Obj("providers[].min_speed"),
                    Def::Unset,
                    t(
                        "End a streamed answer that generates too slowly: from the first token on, once a second, when the average speed over the last `window_secs` is below `tokens_per_sec`, the answer ends with an error clients retry (503, `overloaded_error` for Anthropic clients), and the upstream counts one failure toward `failover.failures_to_pause`. Long pauses and summarized reasoning partway through are left out of the average; the idle timeout deals with pauses. Answers that are not streamed are not judged. Unset: never.",
                        "流式回答生成得太慢时结束它：从第一个 token 起每秒看一次，最近 `window_secs` 秒的平均速度低于 `tokens_per_sec` 时，回答以一条客户端会重试的错误结束（503，Anthropic 格式的客户端收到 `overloaded_error`），这家记一次失败，计入 `failover.failures_to_pause`。中途较长的停顿和摘要式的推理不算进平均速度，停顿由无响应超时处理。不是流式的回答不管。不写：不管。",
                    ),
                ),
                row(
                    "limits",
                    Kind::Objs("providers[].limits[]"),
                    Def::Unset,
                    t(
                        "Usage limits of this upstream, written like its own quota: requests, tokens or cost every few minutes or hours, per day, week or month. Every one has to have room before a request is sent to it. When a rolling window is full, requests go to the next upstream or wait for room; when a day, week or month is used up, requests go to the next upstream until the next one starts. Unset: no limit.",
                        "这家上游的用量上限，照着它自己的限额写：每几分钟、每几小时、每天、每周或每月的请求数、token 数或费用。每一条都有余量才往这家发。滚动窗口满了，请求换下一家或者等它空出来；天、周、月用满了，到下一期之前请求换下一家。不写：不限。",
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
                row(
                    "balance",
                    Kind::Enum(balance_settings),
                    Def::Is("auto"),
                    t(
                        "Where the upstream's balance is read from, with its own key and proxy: `openrouter`, `deepseek`, `moonshot`, `sub2api`, `newapi` (New API and One API) or `thinkwatch` (a ThinkWatch Enterprise gateway). `auto`: recognized from `base_url` for OpenRouter, DeepSeek and Moonshot; any other address, except Anthropic's, OpenAI's and Gemini's own, is asked once whether it is a Sub2API, ThinkWatch Enterprise or New API relay, and the answer is kept in memory only. `off`: no balance. ChatGPT and Z.ai accounts and Bedrock have no balance whatever is written here.",
                        "从哪里读这家上游的余额，用它自己的密钥、走它自己的代理：`openrouter`、`deepseek`、`moonshot`、`sub2api`、`newapi`（New API 和 One API）或 `thinkwatch`（ThinkWatch 企业网关）。`auto`：OpenRouter、DeepSeek、Moonshot 按 `base_url` 识别；其他地址（Anthropic、OpenAI、Gemini 的官方地址除外）询问一次它是不是 Sub2API、ThinkWatch 企业网关或 New API 中转站，结果只保存在内存中。`off`：不读余额。ChatGPT、Z.ai 账号和 Bedrock 不读余额，与这里写什么无关。",
                    ),
                ),
                row(
                    "signed_in",
                    Kind::Enum(signed_in),
                    Def::Unset,
                    t(
                        "Which account sign-in the `key` came from: `zai` (Z.ai) or `bigmodel` (BigModel). Signing in to the account in the desktop app writes it; replacing the key there removes it. It does not change how requests are sent. Unset: the key was written by hand.",
                        "`key` 来自哪一家的账号登录：`zai`（Z.ai）或 `bigmodel`（BigModel）。在桌面端登录账号时写入，在桌面端更换密钥时删除。不影响请求的发送方式。不写：密钥是手动填写的。",
                    ),
                ),
            ],
        },
        Section {
            path: "providers[].model_specs.*",
            // 每项都可选，至少写一项由校验管
            ty: checked!(ModelSpec, "{}"),
            rows: vec![
                row(
                    "context_window",
                    Kind::Int,
                    Def::Unset,
                    t(
                        "Context window: the most tokens a request can take in. Unset: the price table's.",
                        "上下文窗口，即一次请求最多输入多少 token。不写：取价目表的。",
                    ),
                ),
                row(
                    "max_output_tokens",
                    Kind::Int,
                    Def::Unset,
                    t(
                        "The most tokens an answer can have. Unset: the price table's.",
                        "一次回答最多输出多少 token。不写：取价目表的。",
                    ),
                ),
                row(
                    "reasoning",
                    Kind::Bool,
                    Def::Unset,
                    t(
                        "Whether the model reasons. The model list (`GET /v1/models`) carries it, so clients offer reasoning levels for it. Unset: the price table's; the list leaves it out when the price table does not say.",
                        "这个模型会不会推理。模型列表（`GET /v1/models`）带着它，客户端据此给出推理档位。不写：取价目表的；价目表也没写时，列表里不给这一项。",
                    ),
                ),
                row(
                    "image_input",
                    Kind::Bool,
                    Def::Unset,
                    t(
                        "Whether the model takes images as input. The model list carries it, as `input_modalities`. Unset: the price table's; the list leaves it out when the price table does not say.",
                        "这个模型收不收图。模型列表里以 `input_modalities` 给出。不写：取价目表的；价目表也没写时，列表里不给这一项。",
                    ),
                ),
            ],
        },
        Section {
            path: "providers[].min_speed",
            ty: checked!(MinSpeed, "{tokens_per_sec: 15}"),
            rows: vec![
                row(
                    "tokens_per_sec",
                    Kind::Int,
                    Def::Required,
                    t(
                        "The lowest acceptable speed, in tokens per second, counted the way the live speed in the app is. From 1 to 1000.",
                        "最低生成速度，单位 token/秒，和应用里显示的实时速度同一个算法。取值 1 到 1000。",
                    ),
                ),
                row(
                    "window_secs",
                    Kind::Int,
                    Def::Is("20"),
                    t(
                        "How many seconds the average is taken over. The first one is taken this long after the first token. From 5 to 120.",
                        "按最近多少秒的平均速度判断。第一次判断在第一个 token 之后这么久。取值 5 到 120。",
                    ),
                ),
            ],
        },
        Section {
            path: "providers[].limits[]",
            ty: checked!(UsageLimit, "{per: day}"),
            rows: vec![
                row(
                    "per",
                    Kind::Enum(limit_pers),
                    Def::Required,
                    t(
                        "The period. `minute` and `hour` are rolling (the last 60 seconds, the last 60 minutes, or `every` of them); `day`, `week` and `month` start again at local midnight, on Monday and on the 1st.",
                        "按多长一段时间算。`minute`、`hour` 是滚动的（最近 60 秒、最近 60 分钟，或者 `every` 个这样的单位）；`day`、`week`、`month` 在本地时间的零点、周一零点、每月一号零点重新算。",
                    ),
                ),
                row(
                    "every",
                    Kind::Int,
                    Def::Unset,
                    t(
                        "How many minutes or hours the rolling window spans: `{per: hour, every: 5}` is the last 5 hours. From 1 to 60 for `minute`, from 1 to 24 for `hour`; `day`, `week` and `month` do not take it. Unset: 1.",
                        "滚动窗口有几分钟、几小时：`{per: hour, every: 5}` 是最近 5 小时。`minute` 取 1 到 60，`hour` 取 1 到 24；`day`、`week`、`month` 不能写。不写：1。",
                    ),
                ),
                row(
                    "requests",
                    Kind::Int,
                    Def::Unset,
                    t(
                        "At most this many requests sent to this upstream. Each attempt that reaches it counts, unless it answers 429; token counts do not count.",
                        "最多往这家发这么多个请求。每一次发到了这家的尝试都算，它回 429 的不算；数 token 的请求不算。",
                    ),
                ),
                row(
                    "tokens",
                    Kind::Int,
                    Def::Unset,
                    t(
                        "At most this many tokens in the answers of this upstream: uncached input, cache writes and output.",
                        "这家回答的请求最多用这么多 token：未命中缓存的输入、写入缓存的和输出。",
                    ),
                ),
                row(
                    "cost",
                    Kind::Num,
                    Def::Unset,
                    t(
                        "At most this much, in US dollars, as recorded for the requests this upstream answers; at least 0.01. Models without a price count as 0, and so does everything with `billing: free`.",
                        "这家回答的请求最多花这么多美元，按每个请求记下的费用算；至少 0.01。没有价格的模型算 0，`billing: free` 时全都算 0。",
                    ),
                ),
                row(
                    "cache_reads",
                    Kind::Bool,
                    Def::Is("false"),
                    t(
                        "Count cache reads too. Only for a `tokens` limit.",
                        "把读取缓存的 token 也算进去。只有 `tokens` 上限能写。",
                    ),
                ),
            ],
        },
        Section {
            path: "providers[].aws",
            // 字段都可选：写访问密钥还是写 profile，由写法检查管二选一
            ty: checked!(Aws, "{}"),
            rows: vec![
                row(
                    "access_key_id",
                    Kind::Secret,
                    Def::Unset,
                    t(
                        "Access key ID. Written together with `secret_access_key`; `profile` instead.",
                        "访问密钥 ID。与 `secret_access_key` 一起写；也可以改写 `profile`。",
                    ),
                ),
                row(
                    "secret_access_key",
                    Kind::Secret,
                    Def::Unset,
                    t("Secret access key.", "私有访问密钥。"),
                ),
                row(
                    "session_token",
                    Kind::Secret,
                    Def::Unset,
                    t(
                        "Session token of temporary credentials, such as those STS issues. When it expires, requests are refused until it is replaced.",
                        "临时凭证（如 STS 签发的）的会话令牌。过期之后请求会被拒绝，直到换上新的。",
                    ),
                ),
                row(
                    "profile",
                    Kind::Str,
                    Def::Unset,
                    t(
                        "Profile in the AWS credential files to read the access keys from, instead of writing them here: `~/.aws/credentials` and `~/.aws/config`, or the files `AWS_SHARED_CREDENTIALS_FILE` and `AWS_CONFIG_FILE` name, on the machine core runs on. The files are read again when they change.",
                        "从 AWS 凭证文件读访问密钥时用的 profile，代替把密钥写在这里：`~/.aws/credentials` 和 `~/.aws/config`，或 `AWS_SHARED_CREDENTIALS_FILE`、`AWS_CONFIG_FILE` 指定的文件，读的是 core 所在机器上的。文件变了会重新读。",
                    ),
                ),
                row(
                    "region",
                    Kind::Str,
                    Def::Unset,
                    t(
                        "Region to sign for. Unset: the one in `base_url`, which must then be a standard runtime endpoint. Required when `base_url` is a VPC endpoint or a proxy.",
                        "签名用的区域。不写：取 `base_url` 里的区域，这时 `base_url` 必须是标准的推理地址。`base_url` 是 VPC 端点或代理时必须写。",
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
                        "Input price once a request's input, cache reads and writes included, exceeds 200K tokens. Written together with `output_above_200k`, or neither. Cache prices stay the ones above.",
                        "单次请求的输入（连同缓存读写）超过 200K token 后的输入单价。与 `output_above_200k` 同时写或都不写；缓存单价仍按上面写的算。",
                    ),
                ),
                row(
                    "output_above_200k",
                    Kind::Num,
                    Def::Unset,
                    t(
                        "Output price once a request's input, cache reads and writes included, exceeds 200K tokens.",
                        "单次请求的输入（连同缓存读写）超过 200K token 后的输出单价。",
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
                    Def::Is("forward"),
                    t(
                        "Requests that name a session. Forwarded by default: intercepting them gives every session the same title.",
                        "为会话起标题的请求。默认转发：拦下后所有会话都会是同一个标题。",
                    ),
                ),
                row(
                    "topic_detect",
                    Kind::Enum(probe_actions),
                    Def::Is("forward"),
                    t(
                        "Topic detection. Forwarded by default.",
                        "话题检测。默认转发。",
                    ),
                ),
                row(
                    "suggestion",
                    Kind::Enum(probe_actions),
                    Def::Is("forward"),
                    t("Suggestions. Forwarded by default.", "建议。默认转发。"),
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
                        "Outbound redaction (Secrets & privacy in the app): credentials and personal information in the text a request carries (messages, system prompts, reasoning, tool arguments and results, tool descriptions) are replaced with placeholders before it leaves, or only recorded, rule by rule. Identifiers, model names and other protocol fields are sent as they are.",
                        "出站脱敏（应用里叫「密钥与隐私」）：请求发出前，按每条规则把其中正文（消息、系统提示、推理、工具调用的参数和结果、工具说明）里的凭据和个人信息替换为占位符，或只记录；id、模型名等协议字段原样发出。",
                    ),
                ),
                row(
                    "inspect_tools",
                    Kind::Obj("security.inspect_tools"),
                    Def::Section,
                    t(
                        "Tool-call inspection (Dangerous commands in the app): dangerous commands in the tool calls a model returns cut the response off.",
                        "工具调用审查（应用里叫「危险命令」）：模型返回的工具调用中出现危险命令时切断响应。",
                    ),
                ),
                row(
                    "content",
                    Kind::Obj("security.content"),
                    Def::Section,
                    t(
                        "Content filter: words, patterns or characters (hidden ones among them) in what the caller sends, and words or patterns in the text of the answer; each rule refuses the request, deletes or replaces what it matched, or only records it.",
                        "内容过滤：调用方发送的内容中、或模型回答的正文中出现指定的词、写法或字符（包括隐藏字符）时，按规则拒绝请求、删除或替换命中的内容，或只记录。",
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
                    "actions",
                    Kind::EnumMap(RULE_ID, redact_actions),
                    Def::Is("{}"),
                    t(
                        "What a built-in rule does under `enforce`, written only where it differs from the factory setting (`email: record`). `placeholder` sends a placeholder in place of the value and puts the value back where the answer repeats the placeholder; `record` sends the value as it is and records the match.",
                        "内置规则在 `enforce` 下的动作，只写与出厂不同的（`email: record`）。`placeholder` 将值替换为占位符后发送，回答中出现该占位符时还原为原值；`record` 原样发送，记录命中。",
                    ),
                ),
                row(
                    "custom",
                    Kind::Objs("security.redact.custom[]"),
                    Def::Is("[]"),
                    t(
                        "Rules of your own: whatever a rule matches is replaced like a credential, or only recorded.",
                        "自定义规则：匹配到的内容和凭据一样替换，或只记录。",
                    ),
                ),
                row(
                    "exceptions",
                    Kind::Objs("security.redact.exceptions[]"),
                    Def::Is("[]"),
                    t(
                        "Values that are never replaced or recorded, under any rule. Each is kept as a hash, never as the value itself; at most 500. A saved secret cannot be an exception: an entry whose rule is `thinkwatch-saved-secret` is read but has no effect.",
                        "例外值：在任何规则下都不替换、不记录。只保存哈希，不保存值本身；最多 500 个。保存的凭据不能列为例外：规则为 `thinkwatch-saved-secret` 的条目可以读入，但不生效。",
                    ),
                ),
            ],
        },
        Section {
            path: "security.redact.exceptions[]",
            ty: checked!(
                RedactException,
                "{sha256: 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08}"
            ),
            rows: vec![
                row(
                    "sha256",
                    Kind::Str,
                    Def::Required,
                    t(
                        "SHA-256 of the value, as 64 lowercase hexadecimal characters. Escapes in the request body are decoded first: the hash is of the value as written.",
                        "值的 SHA-256，64 个小写十六进制字符。请求体中的转义先解开：哈希按值本身计算。",
                    ),
                ),
                row(
                    "masked",
                    Kind::Str,
                    Def::Is("''"),
                    t(
                        "The value as shown when it was added, masked the same way as in the security log.",
                        "加入时显示的值，打码方式与安全日志相同。",
                    ),
                ),
                row(
                    "rule",
                    Kind::Str,
                    Def::Is("''"),
                    t(
                        "The rule that first recognised the value: a built-in rule id or a custom rule name. Empty for a value added by hand that no rule recognises.",
                        "最先认出该值的规则：内置规则的 id 或自定义规则的名称。手动添加且没有规则认出的值为空。",
                    ),
                ),
                row(
                    "added_at",
                    Kind::Int,
                    Def::Is("0"),
                    t(
                        "When the value was added, in milliseconds since the Unix epoch.",
                        "加入的时间，Unix 纪元以来的毫秒数。",
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
                    t(
                        "The text to look for; a regular expression with `match: regex`.",
                        "要查找的文字；`match: regex` 时为正则表达式。",
                    ),
                ),
                row(
                    "match",
                    Kind::Enum(redact_matches),
                    Def::Is("contains"),
                    t(
                        "`contains`: `pattern` is looked for as written; characters such as `.` and `*` have no special meaning. `regex`: `pattern` is a regular expression.",
                        "`contains`：按原样查找 `pattern`，`.`、`*` 等字符没有特殊含义。`regex`：`pattern` 是正则表达式。",
                    ),
                ),
                row(
                    "case_sensitive",
                    Kind::Bool,
                    Def::Is("false"),
                    t(
                        "Tell capital letters from small ones. Off by default: `ABC` and `abc` both match.",
                        "区分大小写。默认不区分：`ABC` 和 `abc` 都会匹配。",
                    ),
                ),
                row(
                    "label",
                    Kind::Str,
                    Def::Is("SECRET"),
                    t(
                        "Placeholder name: what the rule matches is replaced with `<<TW_label_1>>`, numbered per name. Capital letters, digits and underscores, starting with a letter, at most 24 characters.",
                        "占位符名称：匹配到的内容替换为 `<<TW_名称_序号>>`，每个名称各自编号。只能使用大写字母、数字和下划线，以字母开头，最多 24 个字符。",
                    ),
                ),
                row(
                    "action",
                    Kind::Enum(redact_actions),
                    Def::Is("placeholder"),
                    t(
                        "Under `enforce`: replace what the rule matches with a `placeholder`, or only `record` the match and send it as it is.",
                        "`enforce` 下将匹配到的内容替换为占位符（`placeholder`），或只记录、原样发送（`record`）。",
                    ),
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
                        "What a built-in rule does under `enforce`, written only where it differs from the factory setting. Built-in rules check requests only; `replace` puts `***` in place of a match.",
                        "内置规则在 `enforce` 下的动作，只写与出厂不同的。内置规则只检查请求；`replace` 将命中的内容替换为 `***`。",
                    ),
                ),
                row(
                    "custom",
                    Kind::Objs("security.content.custom[]"),
                    Def::Is("[]"),
                    t(
                        "Rules of your own, each checking requests, answers or both.",
                        "自定义规则，每条可检查请求、回答或两者。",
                    ),
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
                        "A keyword; a regular expression with `match: regex`; code points with `match: codepoints` (`U+200B, U+E0000–U+E007F`).",
                        "关键词；`match: regex` 时为正则表达式；`match: codepoints` 时为码位（`U+200B, U+E0000–U+E007F`）。",
                    ),
                ),
                row(
                    "scope",
                    Kind::Enums(content_scopes),
                    Def::Is("[request]"),
                    t(
                        "What the rule checks: `request` is what the caller sends (its messages and the tool results in them); `response` is the text of the answer the client sees, the input of free-form tools such as Codex's `apply_patch` included, and neither tool calls nor reasoning. Both may be listed. A streamed answer is never held back whole: only the last few characters that could still grow into a match wait for the next piece, at most one fewer than the longest keyword, and at most 256 for a regular expression; a match longer than that may have its beginning sent already. Under `enforce`, rules that delete hold back as many characters again: text that comes together where something was deleted is checked again, until no match is left. A match nested deeper than that may have its beginning sent already, and the rest of it is deleted. Under `enforce`, when any rule that checks answers deletes or replaces, logprobs are removed from streamed answers, and from a whole answer whose text changed, since they repeat the text token by token. A rule that checks answers cannot `block`, and cannot match by `codepoints`.",
                        "检查对象：`request` 为调用方发送的内容（用户消息及其中的工具结果）；`response` 为客户端看到的回答正文，包含 Codex `apply_patch` 等自由格式工具的输入，不含工具调用与思考过程。两项可同时列出。流式回答不整体缓冲，只暂缓发送可能构成命中的末尾片段：「包含」规则不超过最长匹配内容的字符数减一，正则规则最多 256 个字符；超过此长度的正则命中，其起始部分可能已送达客户端。`enforce` 下有删除内容的规则时，再多暂缓同样多的字符：删除处前后拼接成的文字重新检查，直到不再有命中。嵌套层数超出暂缓范围的命中，其起始部分可能已送达客户端，其余部分删除。`enforce` 下有检查回答的规则删除或替换内容时，流式回答中的 logprobs 一律去掉，非流式回答在正文有改动时去掉：logprobs 按 token 重复了正文。检查回答的规则不能用 `block`，也不能按 `codepoints` 匹配。",
                    ),
                ),
                row(
                    "match",
                    Kind::Enum(content_matches),
                    Def::Is("contains"),
                    t(
                        "`contains`: the text contains `pattern` as written; characters such as `.` and `*` have no special meaning. `regex`: `pattern` is a regular expression. `codepoints`: the text has a character among the code points or ranges listed in `pattern`, separated by commas; requests only.",
                        "`contains`：正文按原样包含 `pattern`，`.`、`*` 等字符没有特殊含义。`regex`：`pattern` 是正则表达式。`codepoints`：正文中出现 `pattern` 所列码位或码位范围内的字符，多个之间用逗号分隔；仅用于请求。",
                    ),
                ),
                row(
                    "case_sensitive",
                    Kind::Bool,
                    Def::Is("false"),
                    t(
                        "Tell capital letters from small ones. Off by default: `ABC` and `abc` both match. Code points are what they are either way.",
                        "区分大小写。默认不区分：`ABC` 和 `abc` 都会匹配。码位不受此项影响。",
                    ),
                ),
                row(
                    "action",
                    Kind::Enum(content_actions),
                    Def::Is("record"),
                    t(
                        "Under `enforce`: `block` the request (requests only), `strip` what matched and send the rest, `replace` what matched with `replacement` and send the rest, or only `record` the match. Text that comes together where something was stripped is checked again, until no match is left; a replacement is never checked again, and in an answer a regular expression anchored to the start or end of the text or of a line (`^`, `$`, `\\A`, `\\z`) is checked once. Replacing cannot be undone: the answer does not get the original back.",
                        "`enforce` 下拒绝请求（`block`，仅用于请求）、删除命中的内容后发出（`strip`）、将命中的内容替换为 `replacement` 后发出（`replace`），或只记录（`record`）。删除处前后拼接成的文字重新检查，直到不再有命中；替换上的文字不再检查；回答中锚定正文或行首尾的正则（`^`、`$`、`\\A`、`\\z`）只检查一次。替换不可还原，回答中不换回原文。",
                    ),
                ),
                row(
                    "replacement",
                    Kind::Str,
                    Def::Is("\"***\""),
                    t(
                        "What `replace` puts in place of a match.",
                        "动作为 `replace` 时替换成的文字。",
                    ),
                ),
                row("disabled", Kind::Bool, Def::Is("false"), RULE_DISABLED),
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
                    Def::Is("5368709120"),
                    t(
                        "Upper bound on the bytes bodies may take; beyond it the oldest days go first. The default is 5 GiB.",
                        "正文最多占用的字节数，超出时从最早的日期开始删除。默认 5 GiB。",
                    ),
                ),
            ],
        },
        // ── failover ──────────────────────────────────────────
        Section {
            path: "failover",
            ty: checked!(Failover, "{}"),
            rows: vec![
                row(
                    "failures_to_pause",
                    Kind::Int,
                    Def::Is("3"),
                    t(
                        "Consecutive failures without a stated reason (5xx, connection errors) before the upstream is set aside. From 1 to 100.",
                        "没有说明原因的失败（5xx、连接失败）连续几次后停用这家上游，取值 1 到 100。",
                    ),
                ),
                row(
                    "pause_secs",
                    Kind::Int,
                    Def::Is("60"),
                    t(
                        "Seconds the first such pause lasts. Each further pause doubles it, up to `max_pause_secs`; one success resets it.",
                        "这类失败第一次停用的秒数。之后每停用一次翻一倍，直到 `max_pause_secs`；成功一次后回到这个值。",
                    ),
                ),
                row(
                    "max_pause_secs",
                    Kind::Int,
                    Def::Is("600"),
                    t(
                        "Upper bound on the doubled pause, in seconds; not less than `pause_secs`.",
                        "翻倍后的停用上限，单位秒，不小于 `pause_secs`。",
                    ),
                ),
                row(
                    "no_balance_pause_secs",
                    Kind::Int,
                    Def::Is("1800"),
                    t(
                        "Seconds to set aside an upstream that reports an insufficient balance.",
                        "上游报告余额不足时停用的秒数。",
                    ),
                ),
                row(
                    "quota_pause_secs",
                    Kind::Int,
                    Def::Is("3600"),
                    t(
                        "Seconds to set aside an upstream whose quota is used up when it does not say when the quota resets. When it does, the upstream is set aside until then.",
                        "上游报告额度用完、但没有给出重置时间时停用的秒数。给出了重置时间的，停用到那一刻。",
                    ),
                ),
                row(
                    "rate_limit_max_pause_secs",
                    Kind::Int,
                    Def::Is("3600"),
                    t(
                        "Longest pause, in seconds, after an upstream answers 429 (rate limited). The pause lasts as long as the upstream says (`retry-after-ms`, `Retry-After`, or a delay in the error body); when it says nothing, it backs off by the upstream's consecutive 429s: 5, 10, 20 and 40 seconds, then 60 seconds each time. A pause is not a failure: requests wait for the upstream within `wait_secs`, and the circuit breaker does not count it.",
                        "上游回 429（限流）后暂停这家的最长秒数。上游给出了等待时间的（`retry-after-ms`、`Retry-After` 或错误正文里的等待时间）按它给的暂停；没有给出的，按这家连续被限流的次数退避：5、10、20、40 秒，之后每次 60 秒。暂停不算失败：请求在 `wait_secs` 之内等这家恢复，熔断也不计入。",
                    ),
                ),
                row(
                    "idle_timeout_secs",
                    Kind::Int,
                    Def::Is("300"),
                    t(
                        "Seconds an upstream may go without sending content before the gateway stops waiting for it. Counted from the moment the request is sent and started again by every piece of content: text, reasoning and tool calls count, keep-alives do not. A whole (non-streamed) answer counts from sending to the complete answer. Before any content has reached the client, the upstream counts as failed and the request moves to the next one; with none left, the client gets a timeout error. After content has reached the client, the answer ends with an error. From 30 to 3600.",
                        "上游多少秒没有发出内容，网关就不再等它。从请求发出的那一刻算起，每来一段内容重新计时：正文、推理、工具调用都算，心跳不算。整包（非流式）的回答从发出算到整份回来。还没有内容交给客户端时，这家上游记一次失败，请求换到下一家；没有下一家了，客户端收到超时错误。已经有内容交给客户端的，回答以一条错误收尾。取值 30 到 3600。",
                    ),
                ),
                row(
                    "wait_secs",
                    Kind::Int,
                    Def::Is("120"),
                    t(
                        "Seconds a request waits in all, counted once the key's own `max_concurrent` lets it in: for a key's `minute` or `hour` limit to free up, for an upstream that cannot take it yet — at its `max_concurrent`, paused after answering 429, or at one of its `minute` or `hour` usage limits — and before retrying upstreams that failed temporarily before any content (overloaded, a server error, unreachable). A key limit that does not free up in time refuses the request. An upstream that cannot take it is skipped for the next one, and when none is left the request waits for the first to become available; a conversation that stays on an upstream waits for it. When nothing became available in time, the client gets 429 with `Retry-After`; when retries did not succeed in time, it gets the last upstream error. `0`: never wait or retry. From 0 to 600.",
                        "一个请求合计最多等的秒数，从过了密钥自己的 `max_concurrent` 时算起：等密钥的 `minute`、`hour` 上限空出名额，等暂时不能发的上游 —— 并发数满了（`max_concurrent`）、回了 429 正在暂停、到了这家 `minute` 或 `hour` 的用量上限 —— 和上游在内容之前暂时出错（过载、服务端出错、连不上）后重试之前的等待，都算在里面。密钥的上限到时空不出来就拒绝。上游暂时不能发的先换下一家，没有下一家了就等最先恢复的那一家；留在某一家的对话等这一家。到时都没有恢复，客户端收到带 `Retry-After` 的 429；到时重试仍未成功，客户端收到最后一次的上游错误。`0`：不等，也不重试。取值 0 到 600。",
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
                        "`fallback`: the first healthy member, in order. `select`: the member named in `selected`. `load-balance`: requests are shared out in proportion to the members' weights; a conversation in progress stays where it is. `url-test`: the fastest by measured time from sending a request to the first content of the answer. `cheapest`: the lowest input price.",
                        "`fallback`：按顺序取第一个健康的。`select`：取 `selected` 指定的那个。`load-balance`：请求按成员的权重分；进行中的对话留在原来那一家。`url-test`：按实测从发出请求到回答第一段内容的时间取最快的。`cheapest`：取输入单价最低的。",
                    ),
                ),
                row(
                    "providers",
                    Kind::StrsOrObjs("groups[].providers[]"),
                    Def::Required,
                    t(
                        "Member upstreams, by name; not groups. Each upstream appears once in a group. In a `load-balance` group, a member can be written as `{name, weight}`.",
                        "成员上游的名字，不能是策略组。同一个上游在一个策略组中只出现一次。`load-balance` 组的成员可以写成 `{name, weight}`。",
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
                row(
                    "balance_by",
                    Kind::Enum(balance_bys),
                    Def::Is("weights"),
                    t(
                        "For `load-balance`: what the members' weights are multiplied by. `weights`: nothing; the weights alone. `latency`: faster upstreams get more. `health`: upstreams that fail less get more. `latency-health`: both. Other group types take only `weights`.",
                        "`load-balance` 类型用：成员的权重再乘上什么。`weights`：不乘，只按权重。`latency`：越快的上游分得越多。`health`：越少失败的上游分得越多。`latency-health`：两者都看。其他类型只能是 `weights`。",
                    ),
                ),
            ],
        },
        Section {
            path: "groups[].providers[]",
            ty: checked!(Member, "{name: a}"),
            rows: vec![
                row(
                    "name",
                    Kind::Str,
                    Def::Required,
                    t(
                        "The upstream, by name. A member written as just its name has weight 1.",
                        "上游的名字。只写名字的成员权重为 1。",
                    ),
                ),
                row(
                    "weight",
                    Kind::Int,
                    Def::Is("1"),
                    t(
                        "The member's share of a `load-balance` group's requests, in proportion to the other members' weights. From 1 to 100. Other group types take no weight other than 1.",
                        "成员在 `load-balance` 组中分到的请求份额，与其他成员的权重成比例。取值 1 到 100。其他类型的策略组只能写 1。",
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
                    Kind::StrOrObjs("routes[].rules[].to[]"),
                    Def::Unset,
                    t(
                        "An upstream or a group, by name; `__all__` is every upstream in declared order. Or pinned models: a list of upstreams with the model to send to each, tried in order. Not allowed together with `when.provider_would_be`.",
                        "上游或策略组的名字；`__all__` 表示按声明顺序的全部上游。也可以指定模型：列出上游及发给它的模型，按顺序备用。不能与 `when.provider_would_be` 同时写。",
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
            path: "routes[].rules[].to[]",
            ty: checked!(Pinned, "{provider: a, model: m}"),
            rows: vec![
                row(
                    "provider",
                    Kind::Str,
                    Def::Required,
                    t(
                        "An upstream, by name; not a group. Each upstream appears once in the list.",
                        "上游的名字，不能是策略组。同一个上游在列表中只出现一次。",
                    ),
                ),
                row(
                    "model",
                    Kind::Str,
                    Def::Required,
                    t(
                        "The model name sent to that upstream, as written: aliases do not apply, and no `set.model` changes it. Not allowed together with `set.model` in the same rule.",
                        "发给这个上游的模型名，原样发出：不经过别名，也不受 `set.model` 改写。同一条规则里不能再写 `set.model`。",
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
                        "Requested model, glob (`claude-opus-*`). An upstream model name also matches requests for the aliases that list it; an alias matches only requests for the alias.",
                        "请求的模型，可用通配（`claude-opus-*`）。写上游模型名，也匹配请求列有它的别名的请求；写别名只匹配请求这个别名的。",
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
                        "A client helper request: `assistant_internal` for any of them, or one class (`titling`). Classes set to `intercept` in `client_probes` are answered locally and never reach routing.",
                        "客户端的辅助请求：`assistant_internal` 表示任意一类，也可以写具体的一类（`titling`）。在 `client_probes` 中设为 `intercept` 的类别由本地应答，不会进入路由。",
                    ),
                ),
                row(
                    "time",
                    Kind::OneOrMany,
                    Def::Unset,
                    t(
                        "Local time of the machine core runs on, as a window `[days ]HH:MM-HH:MM`: `mon-fri 09:00-18:00`, `sat,sun 00:00-24:00`, `22:00-06:00`. Several windows: any of them. See below.",
                        "core 所在机器的本地时间落在窗口 `[days ]HH:MM-HH:MM` 里：`mon-fri 09:00-18:00`、`sat,sun 00:00-24:00`、`22:00-06:00`。写几个窗口满足其一即可。见下文。",
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
                    "reasoning",
                    Kind::Enum(set_reasonings),
                    Def::Unset,
                    t(
                        "Reasoning. `off` turns it off: the reasoning settings and the effort are removed, and in the OpenAI formats the effort is set to `none`. A level changes the effort. Only requests that already have reasoning on are changed; requests without reasoning (such as title generation) are left as they are.",
                        "推理。`off` 关闭推理：去掉推理设置和推理强度，OpenAI 的格式改为写 `none`。写一档则改为该档强度。只改写已开启推理的请求，未开启推理的请求（如生成标题）保持不变。",
                    ),
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
        // ── plugins ───────────────────────────────────────────
        Section {
            path: "plugins[]",
            ty: checked!(
                Plugin,
                "{id: a, file: plugins/a.js, sha256: 0000000000000000000000000000000000000000000000000000000000000000}"
            ),
            rows: vec![
                row(
                    "id",
                    Kind::Str,
                    Def::Required,
                    t(
                        "Lowercase letters, digits and hyphens, 1 to 40 characters; unique.",
                        "小写字母、数字和连字符，1 到 40 个字符，不能重复。",
                    ),
                ),
                row(
                    "file",
                    Kind::Str,
                    Def::Required,
                    t(
                        "The plugin's code, relative to this file's directory. It is always `plugins/<id>.js`; the app writes it.",
                        "插件的代码，相对本文件所在的目录。只能是 `plugins/<id>.js`，由应用写入。",
                    ),
                ),
                row(
                    "sha256",
                    Kind::Str,
                    Def::Required,
                    t(
                        "SHA-256 of the approved code, 64 lowercase hexadecimal characters. When the file no longer has this hash, the plugin stops running until the change is approved in the app. The approved code is kept in `plugins/.approved/<id>.js`.",
                        "批准过的代码的 SHA-256，64 个小写十六进制字符。文件的哈希与它不符时插件停止运行，直到在应用里批准这次改动。批准过的代码另存在 `plugins/.approved/<id>.js`。",
                    ),
                ),
                row(
                    "enabled",
                    Kind::Bool,
                    Def::Is("true"),
                    t(
                        "Run the plugin. `false` keeps it installed and out of every request.",
                        "是否运行这个插件。`false`：插件保留，不参与任何请求。",
                    ),
                ),
            ],
        },
        // ── sync ──────────────────────────────────────────────
        Section {
            path: "sync",
            ty: checked!(
                tw_config::sync::Sync,
                "{device_id: 3f9a2c1e-0b1d-4e2f-9a3b-5c6d7e8f9a0b, device_name: Mac mini, target: {type: folder, path: /sync}}"
            ),
            rows: vec![
                row(
                    "device_id",
                    Kind::Str,
                    Def::Required,
                    t(
                        "Generated when sync is turned on. File names at the sync location show its first 8 characters.",
                        "开启同步时生成。同步位置上的文件名里出现的是它的前 8 个字符。",
                    ),
                ),
                row(
                    "device_name",
                    Kind::Str,
                    Def::Required,
                    t(
                        "Shown in the device list of every device that syncs here.",
                        "在每台参与同步的设备的设备列表里显示的名字。",
                    ),
                ),
                row(
                    "encrypt",
                    Kind::Bool,
                    Def::Is("true"),
                    t(
                        "Encrypt what goes to the sync location with the sync passphrase. Every device at one location uses the same setting; change it in the app, which re-uploads the latest version.",
                        "以同步口令加密存到同步位置的内容。同一同步位置上的所有设备采用同一设置；在应用里更改，应用会重新上传最新版本。",
                    ),
                ),
                row(
                    "passphrase",
                    Kind::Str,
                    Def::Unset,
                    t(
                        "The sync passphrase; required when `encrypt` is on, and kept after encryption is turned off: the keyring that says it is off is checked with it. Kept here in the clear, like the other keys in this file.",
                        "同步口令，`encrypt` 开着时必填；关闭加密之后也留着：同步位置上写着「已关闭」的 keyring 要用它核对。和本文件里的其他密钥一样以明文保存。",
                    ),
                ),
                row(
                    "exclude",
                    Kind::Strs,
                    Def::Is("[]"),
                    t(
                        "Upstreams, by name, that are not synced: they are not uploaded and another device's upstream of the same name does not replace them. Routes, aliases and groups that refer to them are not synced either.",
                        "不同步的上游，按名字：不上传，也不被其他设备的同名上游覆盖。引用它们的路由、别名、分组随之不同步。",
                    ),
                ),
                row(
                    "target",
                    Kind::Obj("sync.target"),
                    Def::Required,
                    t(
                        "The sync location. The keyring, `versions/` and `devices/` live directly in it.",
                        "同步位置。`keyring`、`versions/`、`devices/` 直接放在它里面。",
                    ),
                ),
            ],
        },
        Section {
            path: "sync.target",
            ty: Ty::Code {
                fields: target_type,
                same: super::same_as_omitted::<tw_config::sync::Target>,
                parses_without: super::parses_without::<tw_config::sync::Target>,
                minimal: "{type: folder}",
            },
            rows: vec![row(
                "type",
                Kind::Enum(target_types),
                Def::Required,
                t(
                    "Which kind of location; the other fields depend on it, see the three tables below.",
                    "同步位置的种类；其余字段随之不同，见下面三张表。",
                ),
            )],
        },
        Section {
            path: "sync.target (folder)",
            ty: Ty::Code {
                fields: folder_fields,
                same: super::same_as_omitted::<tw_config::sync::Target>,
                parses_without: super::parses_without::<tw_config::sync::Target>,
                minimal: "{type: folder, path: /sync}",
            },
            rows: vec![
                row("type", Kind::Str, Def::Required, t("`folder`", "`folder`")),
                row(
                    "path",
                    Kind::Str,
                    Def::Required,
                    t(
                        "An absolute path on the machine core runs on: a folder that iCloud Drive, Dropbox, OneDrive, Syncthing or a NAS share carries between devices.",
                        "core 所在机器上的绝对路径：由 iCloud Drive、Dropbox、OneDrive、Syncthing 或 NAS 共享在设备之间传送的文件夹。",
                    ),
                ),
                row(
                    "proxy",
                    Kind::Str,
                    Def::Is("direct"),
                    t("Not used for a folder.", "文件夹用不到。"),
                ),
            ],
        },
        Section {
            path: "sync.target (webdav)",
            ty: Ty::Code {
                fields: webdav_fields,
                same: super::same_as_omitted::<tw_config::sync::Target>,
                parses_without: super::parses_without::<tw_config::sync::Target>,
                minimal: "{type: webdav, url: 'https://dav.example.com/ThinkWatch'}",
            },
            rows: vec![
                row("type", Kind::Str, Def::Required, t("`webdav`", "`webdav`")),
                row(
                    "url",
                    Kind::Str,
                    Def::Required,
                    t(
                        "The WebDAV address of the folder. `https://`, with the certificate checked; `http://` only for this machine and the local network.",
                        "文件夹的 WebDAV 地址。`https://`，校验证书；`http://` 只用于本机和局域网。",
                    ),
                ),
                row(
                    "user",
                    Kind::Str,
                    Def::Is("''"),
                    t("User name.", "用户名。"),
                ),
                row(
                    "pass",
                    Kind::Str,
                    Def::Is("''"),
                    t(
                        "Password, or the app password the service issues for WebDAV.",
                        "密码，或者服务为 WebDAV 签发的应用密码。",
                    ),
                ),
                row("proxy", Kind::Str, Def::Is("direct"), PROXY_DOC),
            ],
        },
        Section {
            path: "sync.target (s3)",
            ty: Ty::Code {
                fields: s3_fields,
                same: super::same_as_omitted::<tw_config::sync::Target>,
                parses_without: super::parses_without::<tw_config::sync::Target>,
                minimal: "{type: s3, endpoint: 'https://s3.us-east-1.amazonaws.com', region: us-east-1, bucket: tw-sync, access_key_id: AK, secret_access_key: SK}",
            },
            rows: vec![
                row("type", Kind::Str, Def::Required, t("`s3`", "`s3`")),
                row(
                    "endpoint",
                    Kind::Str,
                    Def::Required,
                    t(
                        "The S3 endpoint. Hosts ending in `amazonaws.com`, `aliyuncs.com` or `myqcloud.com` put the bucket in the host name; every other endpoint (Cloudflare R2, MinIO, an address) puts it in the path. `http://` only for this machine and the local network.",
                        "S3 端点。主机以 `amazonaws.com`、`aliyuncs.com`、`myqcloud.com` 结尾的，存储桶名放进域名；其余端点（Cloudflare R2、MinIO、IP 地址）放进路径。`http://` 只用于本机和局域网。",
                    ),
                ),
                row(
                    "region",
                    Kind::Str,
                    Def::Required,
                    t(
                        "The signing region; `auto` for Cloudflare R2.",
                        "签名用的区域；Cloudflare R2 填 `auto`。",
                    ),
                ),
                row(
                    "bucket",
                    Kind::Str,
                    Def::Required,
                    t("The bucket. Keep it private.", "存储桶，建议设为私有。"),
                ),
                row(
                    "prefix",
                    Kind::Str,
                    Def::Is("''"),
                    t(
                        "A folder inside the bucket. No `..` parts, and not starting with `/`.",
                        "存储桶里的一个前缀（文件夹）。不能有 `..`，不以 `/` 开头。",
                    ),
                ),
                row(
                    "access_key_id",
                    Kind::Str,
                    Def::Required,
                    t("Access key ID.", "Access Key ID。"),
                ),
                row(
                    "secret_access_key",
                    Kind::Str,
                    Def::Required,
                    t("Secret access key.", "Secret Access Key。"),
                ),
                row("proxy", Kind::Str, Def::Is("direct"), PROXY_DOC),
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
            // 内置规则出厂一律替换为占位符（`placeholder`），表里不另列
            out += if zh {
                "| id | 名称 | 出厂 |\n|---|---|---|\n"
            } else {
                "| id | Name | Out of the box |\n|---|---|---|\n"
            };
            for b in tw_guard::redact::rules::BUILTINS {
                let on = if b.on_by_default { yes } else { no };
                let name = if zh { b.name_zh } else { b.name };
                out += &format!("| `{}` | {name} | {on} |\n", b.id);
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
                let name = if zh { &s.name_zh } else { &s.name };
                out += &format!("| `{}` | {name} | `{a}` |\n", s.id);
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
                let name = if zh { &b.name_zh } else { &b.name };
                out += &format!("| `{}` | {name} | {} | {on} | `{a}` |\n", b.id, b.group);
            }
        }
        _ => return None,
    }
    Some(out)
}
