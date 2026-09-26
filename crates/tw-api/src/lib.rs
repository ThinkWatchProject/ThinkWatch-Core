//! 控制面的**契约**。CLI、Tauri UI、第三方都依赖它。
//!
//! 它刻意不含任何 IO —— 只有类型。这样它能被 Tauri 的前端（通过
//! ts-rs 之类的导出）、CLI、和将来的第三方同时依赖，而不会拖上一个
//! HTTP 栈。
//!
//! **取值来自一个固定集合的字段发 slug，界面自己决定怎么称呼它。**
//! 以前策略组类型、磁盘状态这些发的是中文标签，界面只能拿显示文字去做
//! 判断，而这边改一个措辞，那边的判断就悄悄失效了。带着运行时细节的句子
//! （错误、提示、诊断结论）仍然在这里写好，用书面语。

use serde::{Deserialize, Serialize};

/// 给人看的话：码 + 参数 + 英文原句。见 [`tw_types::Msg`]。**协议里凡是
/// 一句给人读的错误或说明，类型都是它**，不是 `String`。
pub use tw_types::Msg;

mod endpoint;
pub mod ep;
#[cfg(feature = "ts")]
pub mod ts;
pub use endpoint::{Endpoint, ErrorBody, Format, Info, Method, fill};

/// 取值是一个固定集合的字段：线上是 slug，类型是枚举。
///
/// **线上仍然是那个词**（`#[serde(rename)]`），导出到前端是字符串字面量的联合，
/// 界面不用再自己收窄一遍 —— 手写的收窄和这边的集合对不上时，多出来的值会
/// 悄悄走进错的分支。`slug()` / `from_slug()` 给存库、拼路径这些要字符串的地方。
macro_rules! slug_enum {
    (
        $(#[$m:meta])*
        pub enum $name:ident {
            $( $(#[$vm:meta])* $v:ident = $s:literal, )+
        }
    ) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[cfg_attr(feature = "ts", derive(ts_rs::TS))]
        pub enum $name {
            $( $(#[$vm])* #[serde(rename = $s)] $v, )+
        }
        impl $name {
            /// 全部取值，按声明的顺序
            pub const ALL: &'static [Self] = &[$(Self::$v),+];
            /// 线上的那个词
            pub fn slug(self) -> &'static str {
                match self {
                    $(Self::$v => $s,)+
                }
            }
            /// 反过来。不在集合里的是 `None`
            pub fn from_slug(s: &str) -> Option<Self> {
                match s {
                    $($s => Some(Self::$v),)+
                    _ => None,
                }
            }
        }
        /// 写出来就是线上的那个词
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.slug())
            }
        }
        /// 和线上的那个词比
        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.slug() == other
            }
        }
        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.slug() == *other
            }
        }
    };
}

// ---------------------------------------------------------------- 取值固定的词
//
// 下面这些是散在各处的字段共用的词表，按主题排。只在一个地方用到的集合写在
// 它的类型旁边。

slug_enum! {
    /// 一个上游怎么收钱。
    pub enum Billing {
        /// 按价目表算，订阅账号也是
        PerToken = "per-token",
        /// 记 $0
        Free = "free",
    }
}

slug_enum! {
    /// 上游说的接口协议。
    pub enum Protocol {
        Anthropic = "anthropic",
        OpenaiChat = "openai-chat",
        OpenaiResponses = "openai-responses",
        Gemini = "gemini",
        /// ChatGPT 账号登录的 Codex 后端
        Chatgpt = "chatgpt",
    }
}

slug_enum! {
    /// 请求体的格式（方言）。
    pub enum Dialect {
        Anthropic = "anthropic",
        OpenaiChat = "openai-chat",
        OpenaiResponses = "openai-responses",
        Gemini = "gemini",
        /// 只有企业版接 Bedrock；桌面版不会发出这个词
        Bedrock = "bedrock",
    }
}

slug_enum! {
    /// 代理的类型。
    pub enum ProxyKind {
        /// 把域名交给代理解析
        Socks5h = "socks5h",
        /// 本地解析 DNS
        Socks5 = "socks5",
        Http = "http",
        Https = "https",
    }
}

slug_enum! {
    /// 代理用不了时怎么办。
    pub enum OnProxyFail {
        /// 请求失败
        Fail = "fail",
        /// 改走直连
        Direct = "direct",
    }
}

slug_enum! {
    /// 一个上游的模型清单从哪儿来。
    pub enum ModelSource {
        /// 上游列出的
        Discovered = "discovered",
        /// 配置里手写的
        Manual = "manual",
        /// 都没有：不知道它有什么
        None = "none",
    }
}

slug_enum! {
    /// 一个上游现在能不能进候选链。
    pub enum Health {
        Ok = "ok",
        /// 熔断中
        Open = "open",
    }
}

slug_enum! {
    /// 熔断器的状态变成了什么。
    pub enum BreakerState {
        /// 熔断中，不进候选链
        Open = "open",
        /// 可以用
        Closed = "closed",
    }
}

slug_enum! {
    /// 代理通不通。
    pub enum ProxyState {
        /// 刚刚检查不通
        Unreachable = "unreachable",
        /// 又通了
        Reachable = "reachable",
    }
}

slug_enum! {
    /// 上游接不接受凭据。
    pub enum AuthState {
        /// 上游拒绝了凭据
        Rejected = "rejected",
        /// 又能用了
        Accepted = "accepted",
    }
}

slug_enum! {
    /// 一个请求失败在哪一方。和 HTTP 响应里的 `x-thinkwatch-error` 同一个词表，
    /// 另外多一个 `internal`。
    pub enum FailureSource {
        Auth = "auth",
        Config = "config",
        Upstream = "upstream",
        Request = "request",
        RateLimited = "rate_limited",
        Denied = "denied",
        /// 网关自己的代码崩掉了
        Internal = "internal",
    }
}

slug_enum! {
    /// 一项防护的档位。
    pub enum GuardMode {
        Off = "off",
        /// 只记录
        Observe = "observe",
        /// 拦截
        Enforce = "enforce",
    }
}

slug_enum! {
    /// 安全日志的一条做了什么。
    pub enum SecurityOutcome {
        /// 只记录
        Recorded = "recorded",
        /// 已替换成占位符
        Replaced = "replaced",
        /// 已切断
        Cut = "cut",
        /// 请求被拒，没有发出去
        Blocked = "blocked",
    }
}

slug_enum! {
    /// 内容规则怎么认。
    pub enum ContentMatch {
        /// 不分大小写的子串
        Contains = "contains",
        Regex = "regex",
    }
}

slug_enum! {
    /// 出站脱敏找到的东西属于哪一类。
    pub enum SecretKind {
        ApiKeys = "api-keys",
        PrivateKeys = "private-keys",
        Jwt = "jwt",
        ConnStrings = "conn-strings",
        /// 内网地址
        Internal = "internal",
        /// 自定义规则找到的
        Custom = "custom",
    }
}

slug_enum! {
    /// 藏匿字符的藏法。
    pub enum HiddenKind {
        /// 零宽字符
        ZeroWidth = "zero_width",
        /// Unicode 标签字符
        Tag = "tag",
        /// 双向控制符
        Bidi = "bidi",
        /// 同形异义字
        Homoglyph = "homoglyph",
        /// 私用区
        PrivateUse = "private_use",
    }
}

slug_enum! {
    /// 一次账号登录走到哪儿了。
    pub enum LoginStatus {
        Pending = "pending",
        Done = "done",
        Failed = "failed",
        Expired = "expired",
        Cancelled = "cancelled",
    }
}

slug_enum! {
    /// ChatGPT 在哪台设备上授权。
    pub enum ChatgptLoginMode {
        /// 这台机器的浏览器
        Browser = "browser",
        /// 把码输到另一台设备上
        Device = "device",
    }
}

slug_enum! {
    /// ChatGPT 套餐里认得的那些词，和 Codex 客户端认的是同一份。
    ///
    /// **这个集合不归我们定**：OpenAI 一直在加（`go`、`prolite` 都是后来才有的），
    /// 认不出来的词见 [`ChatgptPlan::Other`]。
    pub enum KnownChatgptPlan {
        Free = "free",
        Go = "go",
        Plus = "plus",
        Pro = "pro",
        ProLite = "prolite",
        ProMax = "promax",
        Team = "team",
        Business = "business",
        SelfServeBusinessProLite = "self_serve_business_prolite",
        SelfServeBusinessUsageBased = "self_serve_business_usage_based",
        /// 后端也写成 `hc`
        Enterprise = "enterprise",
        Ent26 = "ent26",
        EnterpriseCbpAutomation = "enterprise_cbp_automation",
        EnterpriseCbpUsageBased = "enterprise_cbp_usage_based",
        /// 后端也写成 `education`
        Edu = "edu",
        EduPlus = "edu_plus",
        EduPro = "edu_pro",
    }
}

/// ChatGPT 账号的套餐。
///
/// **线上就是后端给的那个词**：认得的是 [`KnownChatgptPlan`]，认不出来的原样给。
/// 集合是 OpenAI 的，它随时会多出一个新名字 —— 把新名字说成「未知」，比照原词
/// 写出来更差。前端的类型是 `KnownChatgptPlan | string`。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(untagged)]
pub enum ChatgptPlan {
    Known(KnownChatgptPlan),
    /// 认不出来的词，原样
    Other(String),
}

impl ChatgptPlan {
    /// 读后端给的词。大小写不论，同一个套餐的另一种写法归到一个词上（`hc` 就是
    /// `enterprise`，`education` 就是 `edu`）；空的是 `None`
    pub fn from_raw(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        let lower = raw.to_ascii_lowercase();
        let word = match lower.as_str() {
            "hc" => "enterprise",
            "education" => "edu",
            w => w,
        };
        Some(match KnownChatgptPlan::from_slug(word) {
            Some(k) => Self::Known(k),
            None => Self::Other(raw.to_string()),
        })
    }
}

slug_enum! {
    /// 登哪一家的账号。
    pub enum ZaiFamily {
        Zai = "zai",
        Bigmodel = "bigmodel",
    }
}

slug_enum! {
    /// 一版配置是谁写的。
    pub enum ConfigOrigin {
        /// 界面
        Ui = "ui",
        /// 命令行
        Cli = "cli",
        /// 别的程序或者编辑器
        External = "external",
        /// 回滚
        Rollback = "rollback",
        /// OAuth 凭据轮换之后写回
        Rotation = "rotation",
    }
}

slug_enum! {
    /// 配置错在哪一层。
    pub enum ConfigStage {
        /// YAML 写坏了
        Syntax = "syntax",
        /// 字段名或取值不对
        Schema = "schema",
        /// 单看每个字段都对，合起来不成立
        Semantics = "semantics",
    }
}

slug_enum! {
    /// 价目表从哪儿来。
    pub enum PricingSource {
        /// 随版本内置
        Builtin = "builtin",
        /// 联网刷新过
        Fetched = "fetched",
        Empty = "empty",
    }
}

slug_enum! {
    /// 客户端自己发的辅助请求是哪一类。
    pub enum ProbeClass {
        HealthCheck = "health_check",
        Warmup = "warmup",
        Titling = "titling",
        TopicDetect = "topic_detect",
        Suggestion = "suggestion",
    }
}

slug_enum! {
    /// 一类辅助请求怎么处理。
    pub enum ProbeMode {
        /// 网关自己答
        Intercept = "intercept",
        /// 照常走路由
        Route = "route",
        /// 原样转发
        Passthrough = "passthrough",
    }
}

slug_enum! {
    /// 路由规则 `when` 里的键。
    pub enum ConditionField {
        Model = "model",
        Client = "client",
        Dialect = "dialect",
        InputTokens = "input_tokens",
        MaxTokens = "max_tokens",
        ToolCount = "tool_count",
        Intent = "intent",
        ProviderWouldBe = "provider_would_be",
        Cache = "cache",
        Tools = "tools",
        Image = "image",
        Thinking = "thinking",
        Stream = "stream",
    }
}

slug_enum! {
    /// 路由规则 `set` 里的改写。
    pub enum SetField {
        /// 换模型，整个 prompt cache 作废
        Model = "model",
        MaxTokens = "max_tokens",
        Thinking = "thinking",
        /// 以上改写只在新会话开始时应用
        OnlyAtSessionStart = "only_at_session_start",
    }
}

slug_enum! {
    /// 一个上游为什么服务不了这个模型。
    pub enum ServeSkip {
        /// 上游停用了
        Disabled = "disabled",
        /// 不在启用范围里
        OutOfScope = "out_of_scope",
        /// 模型清单里没有
        NotOffered = "not_offered",
    }
}

/// core 发得出的每一个消息码（[`Msg::code`]），一行一个，按字母排。
///
/// **界面按码翻译，所以这就是它要翻的全部。**桌面端从钉着的那个 tag 读它，和
/// 自己的译文表对一遍：清单里有、表里没有的，中文界面上就是英文。
///
/// `#` 开头的行是说明。码后面可以跟一个标记：`passthrough` 是句子只有占位符
/// （系统或上游的原话），照 `text` 显示；`test` 是只在测试里出现的。这份清单由
/// twcore 的 `tests/msg_codes.rs` 从源码生成、并且在 CI 上核对。
pub const MSG_CODES: &str = include_str!("../msg-codes.txt");

/// 控制面协议版本。UI 和 CLI 连上来时检查，不匹配就明确提示「请升级
/// 客户端」，而不是以奇怪的方式失败。
///
/// **2 起，凡是给人看的一句话都是 [`Msg`]，不是 `String`。**错误响应的
/// 响应体也从纯文本变成了 JSON。照 1 写的客户端会把这些当字符串显示，
/// 屏幕上是一坨 JSON —— 所以这里要跳号，让它在连上的那一刻就失败。
///
/// **3 把剩下的那些也换了。**2 只管了错误：扫描发现、客户端诊断、接管
/// 的代价和提醒仍然是 `String`，于是中文界面上整整三页变成了英文。
/// 换句话说，2 只做了一半 —— 而「一句给人读的话」和「它是不是错误」
/// 本来就没有关系。
///
/// **4 把安全改成了全局的。**上游不再有信任级别和脱敏类别，路由规则不再有
/// 安全要求；两项防护各有自己的规则和日志，出站检测的两条事件合成了一条。
/// 照 3 写的界面会去读已经不存在的字段，所以跳号。
///
/// **5 去掉了全局并发上限**（`limits.max_concurrent`），`GET /keys` 改给明文，
/// 状态里的监听地址跟着真实的监听器走。照 4 写的界面会画出一个空的「全局」格子。
///
/// **6 概览带上了配置版本号，协议里不再有为老版本留的默认值。**照 5 写的
/// 界面从别处读版本号；而缺了这些字段的 core 现在连概览都解析不了。
///
/// **8 额度的重置时间换成了时刻**（`resets_at_ms`，不再是 `reset_in_secs`）：
/// 秒数是收到响应那一刻的，存下来原样再给出去就是一个不会走的倒计时。另外
/// 概览带上了上游凭据被拒、代理不通的现状，请求详情能取还在跑的请求，掉队的
/// 事件流订阅者会收到一条 `EventsDropped`。照 7 写的界面拿不到倒计时。
///
/// **7 把手动配置的说明拆成了步骤、字段和地址**（`ManualClient.how` 没了，
/// 换成 `setup`；能接管的客户端也带上了 `manual`），客户端的最近一次请求改按
/// 为它生成的密钥算。照 6 写的界面会把手动配置那一栏画成空白。
///
/// **9 网关不再有自己的并发上限**：概览里的 `limits` 没了，只剩每把密钥
/// 自己的 `max_concurrent`，超出的请求等着、不再被拒（失败来源里没有了
/// `overloaded`）。放行网段不再有「空 = 私网段」：不写是默认名单、空就是
/// 只有本机，`ListenView` 带上了默认名单；`GET /interfaces` 每张网卡一行。
/// 照 8 写的界面会画出一节改了也不起作用的并发设置。
///
/// **10 计费只剩两档**：`billing` 只有 `per-token` / `free`，订阅账号也按
/// 价目表算费用。上游视图的 `billing` 必有，`billing_effective` 没了；汇总的
/// `subscription_requests` / `subscription_tokens` 和会话的 `subscription_turns`
/// 没了。照 9 写的界面会去读已经不存在的字段。
/// **11 推理测速的错误是 [`Msg`]、输出上限可以是空。**ChatGPT 账号那种上游不接受
/// 输出上限，报价里的上限和金额都是空的；错误原来是一句英文字符串，中文界面上
/// 只能原样显示。照 10 写的界面会把错误画成一个对象。
///
/// **12 起控制面要凭据。**没带 `Authorization: Bearer` 的请求一律 401，
/// 连 `/status` 都读不到 —— 所以照 11 写的客户端**看不到这次跳号**，它先
/// 撞上的是 401。跳号在这里仍然要记，因为这一行是这个协议的变更史，而
/// 「什么时候开始要凭据」是读它的人必须查得到的一件事。
///
/// 同一版加了 `POST /shutdown`。新增端点本身不破坏什么，它跟着这次走。
///
/// **13 起端点只在 [`ep`] 里写一次**，core 按它注册、客户端按它拼。顺带：扫描
/// 换成了 `POST /scan`（项目目录走请求体，查询串里那种写法 core 一直读不了）；
/// 框架替我们回的失败（路径不存在、方法不对、请求体读不成）也是 [`ErrorBody`]，
/// 不再是纯文本或空响应体；传输地址改名 `control::Address`。
///
/// **14 起取值固定的字段是枚举，三处原因是 [`Msg`]。**线上的词没变（防护、策略组
/// 类型、模型清单状态、生效时机、尝试结果、规则动作、发现等级、测速的步骤），
/// 变的是类型；尝试链每一跳的 `error`、模型清单的 `model_error` / `error`、
/// 检测上游的 `error` 原来是一句英文字符串，中文界面上只能原样显示。照 13 写的
/// 界面会把这三处画成一个对象。
///
/// **15 把剩下的固定取值也换成了枚举，凭据和账号登录的原因是 [`Msg`]。**线上的词
/// 没变（计费方式、协议、格式、代理类型、档位、熔断和代理状态、失败来源、登录
/// 状态、配置来源、试算的结论和明细……）；换了类型的原因：`CredentialRotated` /
/// `CredentialExpired` 的 `detail`、`OAuthView.failure`、三处登录的 `error`、
/// `ConfigRejected.message`、`RuleTrace.error`。照 14 写的界面会把这些画成一个对象。
///
/// **16 把最后几处原因也换成了 [`Msg`]**：`KeySyncFailed.error`、`PricingStatus.error`、
/// `TurnView.error`。照 15 写的界面会把它们画成一个对象。
///
/// **17 起「此刻有什么不对」都能问到**，不必从事件流开头听起：`Status.config_rejected`
/// （磁盘上那份配置没通过校验、旧的还在服务）和 `ProviderView.writeback_failed`
/// （换发的凭据没能写回配置）。半路才连上的一方（桌面端的提醒）按它们对账。
///
/// **18 起每条控制面连接先握手，Bearer 头没了。**握手是 Noise
/// （`tw-link`），钥匙是配置里的 `listen.control.key`，`TW_CONTROL_TOKEN` 和
/// `control.token` 都不在了。和 12 一样，照 17 写的客户端**看不到这次跳号**：
/// 它连 HTTP 都说不上，握手第一条就被拒。版本号从这一版起在握手里交换
/// （`tw_link::ClientHello.proto`），不一致时握手就说，不必等到 `/status`。
/// 同一版起 `GET /config` 发出的正文里钥匙是打码的（[`control::KEY_MASK`]）。
///
/// **19 起客户端接管、MCP、扫描不在控制面上**：它们改的是桌面端那台机器上的文件，
/// 由桌面端自己做。`/scan`、`/clients`、`/clients/plan`、`/clients/adopt`、
/// `/clients/{id}/restore/plan`、`/clients/{id}/restore`、`/clients/{id}/why`、
/// `/mcp/*` 和 `ClientsChanged` / `ScanAlert` 两个事件都删了，只留
/// `POST /clients/{id}/key`；更换密钥不再同步客户端的配置（`KeyRotated` 没有
/// `synced` / `failed` 了），删密钥也不再查它是不是写在一个接管着的客户端里。
///
/// **20 加了远程控制端口**（`listen.control.remote`）。`Status` 多了
/// `remote_control`（开没开、听在哪、为什么没听上）和 `gateway_reachable`
/// （别的机器连网关用哪几个地址）。从远程端口进来的连接不能关 core、不能
/// 取诊断包、不能改 `listen.control` 这一节（403，`control.remote.*`）。
/// 照 19 写的客户端会缺这两个字段。
///
/// **21 起账号上游登的是谁在上游视图里**：`OAuthView.account`（邮箱、套餐）从凭据
/// 自己的 access token 里读，不联网；`ChatgptUsage` 不再带 `email` 和 `plan`。套餐是
/// [`ChatgptPlan`]：认得的词是枚举，认不出来的原样给，登录结果里的 `plan` 也换成了它。
/// 照 20 写的界面会去用量里找邮箱和套餐，而那里已经没有了。
///
/// 同一版起**路由和会话在请求开始时就说清楚**：`RequestStarted` 的 `session_fp`
/// 换成了 `session`（落库的那个会话 id，由网关在开始时定），并带上 `route`、
/// `rule`、`group`、`rewritten_by`；`RequestRouted` 带上 `route`、`rewritten_by`、
/// `denied_by`，规则拒绝了的请求也发。`GET /in-flight` 不再是开始事件的数组，是
/// [`InFlight`]：core 的时钟加上每个在跑的请求到目前为止的事件。`RunningView`
/// 多了 `elapsed_ms`、`session`、`route`、`rule`、`group`、`upstream`。新增
/// `GET /summary/routes`。照 20 写的界面会把 `/in-flight` 当成数组去读。
///
/// **22 起命中数说清楚记录从哪一刻起是全的**：`GET /summary/routes` 不再是
/// [`RouteHits`] 的数组，是 [`RouteStats`] —— `routes` 加上 `covered_since_ms`。库刚建好
/// （新装、升级时重建）、记录留的天数比窗口短时，它比问的起点晚；库里没有记录时是空。
/// ChatGPT 登录的结果（[`ChatgptLoginStatus`]）不再带 `plan`，换成 `account`：和
/// [`OAuthView::account`] 同一块（邮箱、套餐），从同一个 access token 里读。照 21 写的
/// 界面会把 `/summary/routes` 当成数组去读，在登录结果里找不到套餐。
///
/// **23 起额度也来自 GLM Coding Plan**（Z.ai / BigModel 的上游）：窗口多了 `monthly`，
/// 积分制套餐的窗口带上 [`QuotaWindow::credits`]（总额、已用、剩余）。照 22 写的界面
/// 不认 `monthly`，也看不到剩余积分。
///
/// 同一版起**记录说得出请求带没带 DeepSeek Harness 的会话日志**：`RequestStarted` 和
/// [`HistoryRow`] 多了 `session_log_bytes`（`dsh_session_log` 的字节数，没带的没有）。
/// 照 22 写的界面看不到它。
///
/// **24 起 GLM 的额度不再数 MCP 工具的调用**：老套餐额度接口里 `TIME_LIMIT` 那一项数的是
/// Z.ai 自家 MCP 工具（search-prime、web-reader、zread）的调用次数，这些调用不经过网关，
/// core 不再读它 —— 窗口的词表里没有 `monthly` 了，模型请求上的 1310 只标 `weekly`，
/// [`QuotaWindow::credits`] 只有积分制套餐（`CREDIT_LIMIT`）的窗口才有。同一版起
/// **额度撤下也有事件**：GLM 的 key 被判定没有套餐、core 清掉这一家的额度时，报一条
/// `windows` 为空的 [`Event::QuotaSeen`]，界面当场收起那一格，不用等下一次读 `/quota`。
/// 照 23 写的界面不知道空的 `QuotaSeen` 是撤下，还给一个再也不会来的 `monthly` 留着位置。
pub const CONTROL_API_VERSION: u32 = 24;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Status {
    pub api_version: u32,
    /// 二进制的 CalVer
    pub version: String,
    pub pid: u32,
    /// 数据面**此刻**在监听哪儿：配置里写的那个地址（绑网卡时顺带开着的
    /// 回环不在这里）。安全模式下是 None —— 那正是「只起控制面」的可观测形态。
    ///
    /// **跟着真实的监听器走。**换了端口、网关已经在新端口上服务之后，这里
    /// 就是新的；换不成的时候这里仍是旧的，原因在 `listen_error`。
    pub gateway_addr: Option<String>,
    /// 配置里的监听地址没能换上的原因（端口被占、网卡没有地址）。**这时
    /// 旧的地址还在服务**，也就是 `gateway_addr` 说的那个
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_error: Option<Msg>,
    /// 磁盘上的配置文件最近一次改动没通过校验：**旧的那一版还在服务**。
    /// 下一次换入成功（改好了、或者界面写了一版）就没有了。和 `ConfigRejected`
    /// 事件说的是同一件事 —— 事件是那一刻，这里是现在
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_rejected: Option<ConfigRejection>,
    pub config_path: String,
    pub clients: usize,
    pub providers: usize,
    pub uptime_secs: u64,
    /// 正在服务中的请求数：从进入网关到响应体最后一个字节发完为止，排队
    /// 的和还在流式输出的都算。
    ///
    /// 重启网关之前要看它 —— 重启会掐断所有还没结束的流。
    pub in_flight: usize,
    /// 远程控制端口此刻的样子。
    pub remote_control: RemoteControlView,
    /// 别的机器连网关该用的地址（`地址:端口`），**不含回环**。
    ///
    /// 网关绑在一张网卡上时就是那一个；绑 `all` 时是这台机器每张网卡的地址
    /// （每张一个，有 IPv4 用 IPv4）；只绑回环时是空的 —— 别的机器根本连不上。
    ///
    /// **core 不知道对方是从哪条路过来的**（NAT、端口转发、域名都看不见）。
    /// 桌面端连远程 core 时，优先用它自己连控制面时拨的那个主机加上网关的
    /// 端口（`gateway_addr` 里的端口）：那个主机名已经被证明从那台 Mac 上
    /// 连得通。这里的清单是给它核对和兜底用的。
    pub gateway_reachable: Vec<String>,
}

/// 远程控制端口（`listen.control.remote`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RemoteControlView {
    /// 配置里开着吗
    pub enabled: bool,
    /// **此刻真的在听的地址**。开着却是空的，原因在 `error`；换端口没换成时
    /// 这里仍是旧的那个
    pub addr: Option<String>,
    /// 配置里的地址没能听上的原因（端口被占、网卡没有地址）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Msg>,
    /// 放行哪些来源。本机永远放行
    pub allow_from: Vec<String>,
    /// 别的机器连这个端口用的地址（`地址:端口`），规则同 `Status.gateway_reachable`
    pub reachable: Vec<String>,
}

/// 一次请求的观测事件。UI 的实时列表吃这个。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// 请求进来了
    RequestStarted {
        id: u64,
        /// 鉴权认出来的身份（网关密钥对应的那个 client 条目）。**不可伪造。**
        client: String,
        /// 请求头透出来的旁证。**可以伪造，所以只用来显示和判断
        /// 「接管生效了吗」，绝不用来鉴权或路由**（见 tw_gateway::hint）。
        ///
        /// 它存在的理由：目标用户「一个 key 就够」，那时五个客户端
        /// 的 `client` 是同一个值，而观察窗口要回答的偏偏是「Codex 那边
        /// 生效了吗」。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_hint: Option<String>,
        /// 这次请求归到哪一次会话（任务）。**落库时 `requests.session` 就是这个值**，
        /// 和 `SessionView.id` 是同一个。
        ///
        /// 由网关在开始的那一刻定，只定一次：同一段对话（system prompt 和第一句话
        /// 的指纹）、离这段对话的上一个请求不超过半小时，就还是那一次；隔久了算
        /// 新的一次。**开始时就给出来**，界面才能把一个还在跑的请求放进它的会话、
        /// 把那次会话标成进行中。认不出会话的没有：正文里没有任何能认人的东西，
        /// 或者是 WebSocket 升级（升级请求没有正文）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        /// 请求从哪台机器来：**这条连接对面的地址**，不可伪造。本机（回环）
        /// 来的不记 —— 那是绝大多数请求，写上只是噪音；局域网来的才值得
        /// 说一句「来自 192.168.1.23」。几台机器共用一把密钥时，只有它分得开
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peer: Option<String>,
        /// 请求带的那把网关密钥打码后的样子（`tw-re…wb4e`）。**记的是请求那一刻
        /// 用的那把** —— 密钥更换过之后，老记录上的尾巴照样对得上
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key_masked: Option<String>,
        /// 走的哪条路由：这把密钥指定的那条，没指定的是默认路由。
        ///
        /// **路由的第一阶段在开始之前就走完了**，所以这几项（路由、规则、策略组、
        /// 改写）开始时就有：界面不必按密钥现在的配置去猜这个请求走的是哪条，
        /// 那份配置在请求开始之后可能已经改过了。`RequestRouted` 带着同样的几项，
        /// 外加第二阶段和尝试链的结果
        route: String,
        /// 决定去向的那条规则：路由里第一条命中的转发或拒绝
        rule: String,
        /// 规则把请求交给的策略组。规则直接指上游、或者拒绝了它时没有
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<String>,
        /// 附加了参数改写的规则，按求值的顺序（`rule` 自己带着改写时也在里面）。
        /// 选定上游之后才判断的那些要等 `RequestRouted`
        rewritten_by: Vec<String>,
        provider: String,
        /// `provider` 那一家怎么收钱，和 `RequestRouted::billing` 同一套词。
        ///
        /// **算数的是 `RequestRouted` 报的那个**（最终服务的那家）。这里先报
        /// 一个，是因为不是每个请求都等得到那一条：上游应答之前客户端就走了
        /// 的、WebSocket 升级没完成就断开的，手上只有这一个 —— 少了它，那一行
        /// 说不出该按什么记账。
        billing: Billing,
        /// 客户端要的模型名。**成本要靠它查价**，而它只在请求体里 ——
        /// 少了这个字段，落库那一步就只能记一笔没有模型的账
        model: String,
        method: String,
        path: String,
        /// 请求带着 DeepSeek Harness 的会话日志（`dsh_session_log`）：这是它序列化之后
        /// 的字节数。没带是 None。
        ///
        /// **会话日志是整段对话**（工作目录、系统提示、每一轮的输入输出、工具的参数和
        /// 结果），默认开着，每个请求补上一段，单次最多 8 MiB。它不进模型输入，只有
        /// DeepSeek 官方收：发给别的上游之前网关会去掉它，界面要能说出「这个请求带着
        /// 会话日志、有多大」
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_log_bytes: Option<u64>,
        at_ms: u64,
    },
    /// 收到上游响应头。**这个事件单独存在是有意的**：流式请求从这里
    /// 到结束可能还有好几分钟，UI 要能在这个点就把行画出来并显示
    /// 「进行中」，而不是等它结束才出现。
    RequestHeaders { id: u64, status: u16, ttfb_ms: u64 },
    /// 结束了
    RequestFinished {
        id: u64,
        /// 客户端要的模型名，和 `RequestStarted` 里的是同一个。
        ///
        /// **三种结局都自己带着它。**听事件的一方不一定从请求开始时就在听
        /// —— 界面的窗口开着才订阅，概览打开时才挂上实时曲线 —— 而用量是
        /// 在结局里才到的。模型名只在开始事件里的话，一个开始时没人在听、
        /// 结束时有人在听的请求，它的用量就不知道该记在哪个模型上。
        ///
        /// WebSocket 那条路是空串：升级请求里没有模型名（和开始事件一样）。
        model: String,
        status: u16,
        bytes: u64,
        duration_ms: u64,
        /// 上游报的用量。**没报就是 None，不是零** —— 零会让一次真实的
        /// 调用看起来是免费的
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<UsageView>,
    },
    /// 失败了。`source` 和 HTTP 响应里的 `x-thinkwatch-error` 是同一个词表
    /// （`auth` / `config` / `upstream` / `request` / `rate_limited` /
    /// `denied`），另外多一个 `internal`：网关自己的代码
    /// 崩掉了。它只出现在这里 —— 那时往往已经没有一个 HTTP 响应能带上它。
    RequestFailed {
        id: u64,
        /// 模型名。理由见 `RequestFinished::model`
        model: String,
        source: FailureSource,
        message: Msg,
        /// 失败之前从上游收到了多少字节。**响应头都没到的没有**
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bytes: Option<u64>,
        /// 从请求进来到失败用了多久。「试过三家、二十秒后放弃」和「立刻被拒」
        /// 是两件事
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// 失败之前嗅到的用量。
        ///
        /// **流断在中间、或者被防火墙切断时，上游已经为它计了费** —— 输入
        /// 全额，输出算到断开为止。不带上它，那笔钱就不在账上。和取消一样，
        /// 按它算出来的钱只能是估算。响应头之前就失败的没有用量。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<UsageView>,
    },
    /// 客户端没等到响应结束就走了（Claude Code 里按一下 Esc）。
    ///
    /// **不是失败，也不是正常结束，所以单独一个事件。**上游那时已经在计费
    /// 了 —— 输入全额，输出算到断开为止 —— 所以它带着到那一刻为止看到的
    /// 用量，存储层照样算钱。而它不能算进失败：上游什么都没做错，记成失败
    /// 会让一个常按 Esc 的用户看到一家「经常出错」的上游。
    ///
    /// **用量停在断开那一刻。**输入通常是齐的（`message_start` 在流的最
    /// 前面），输出多半不是 —— Anthropic 只在流的末尾报累计输出，之前手里
    /// 那个数是个占位。按它算出来的钱只能是估算，而且只会偏低。
    ///
    /// 客户端也可能在响应头到达之前就走了（非流式请求、慢的中转站），那时
    /// 没有状态码、没有字节、也没有用量。
    RequestCancelled {
        id: u64,
        /// 模型名。理由见 `RequestFinished::model`
        model: String,
        /// 上游的响应头还没到就走了的，**没有状态码** —— 不是 0
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        /// 断开之前从上游收到了多少字节
        bytes: u64,
        duration_ms: u64,
        /// **没嗅到就是 None，不是零** —— 客户端可能在第一帧之前就走了
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<UsageView>,
    },
    /// 路由决定完了，尝试链也走完了。
    ///
    /// **单独一个事件，因为成功和失败两条路都要发它。**挂在
    /// `RequestFinished` 上的话，失败的那条路就没有尝试链 —— 而那恰恰
    /// 是最需要看它的时候。
    ///
    /// WebSocket 那条路也发：和上游的握手有了结果就发。那条路不做故障转移，
    /// 尝试链只有一跳。
    ///
    /// **规则做了决定、请求却一家上游都没到的也发**：规则拒绝了它（第一阶段），
    /// 或者规则选中的上游都服务不了这个模型 —— 那时尝试链是空的，紧跟着一条
    /// 失败。每条规则命中了多少请求要数得到它们（见 `GET /summary/routes`）。
    ///
    /// 带着的是这次请求**完整的路由记录**，落库的就是它（[`RoutingView`]）：开始
    /// 事件里那几项（路由、规则、策略组、改写）在这里再给一遍，是这一份的终稿 ——
    /// 改写多了第二阶段的，还有第二阶段的拒绝。
    RequestRouted {
        id: u64,
        /// 走的哪条路由，同 `RequestStarted::route`
        route: String,
        /// 命中了哪条规则。**日志和界面都要显示它** —— 「命中第 4 条」
        /// 远不如「命中『带缓存的必须走官方』」有用
        rule: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<String>,
        /// 附加了参数改写的规则：开始事件里的那几条，加上选定上游之后才判断、
        /// 又改写了它的（故障转移时每一跳各判断一次，合在一起，不重复）
        rewritten_by: Vec<String>,
        /// 选定上游之后才判断的规则拒绝了这次请求：是哪一条。**这时尝试链的最后
        /// 一跳就是被拒的那一家**（没有发出去）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        denied_by: Option<String>,
        /// 试过哪几家、各自什么结果。**一次就成的也有一条** ——
        /// 「只试了一家」和「试了三家」在用户眼里应该是不同的
        attempts: Vec<AttemptView>,
        /// 最终服务的那家怎么收钱。
        ///
        /// **必须跟着这次请求走，不能事后查配置** —— 配置随时会被热重载，
        /// 而一条三天前的记录该按它当时那家的计费方式算。
        ///
        /// 一家都没接下时是 `per-token`：没有哪一家的计费方式可以跟着走。
        billing: Billing,
    },
    /// 调用方发来的正文里（连同工具结果）有藏起来的字符：标签字符或双向控制符。
    ///
    /// **观察档和拦截档报的是同一条**，差别只在 `blocked`：拦截档下这个请求没有
    /// 发出去，随后是一条来源为 `denied` 的失败。
    HiddenTextFound {
        id: u64,
        /// 这时要发往的上游（故障转移之前的首选）
        provider: String,
        /// 请求被拒了吗。`false` = 观察档，只记录
        blocked: bool,
        items: Vec<HiddenItem>,
        at_ms: u64,
    },
    /// 调用方发来的正文里（连同工具结果）命中了内容规则。**一条规则一条事件**。
    ContentMatched {
        id: u64,
        provider: String,
        /// 内置规则的 id，或者自定义规则的名字
        rule: String,
        custom: bool,
        /// 这条规则在拦截档下做什么：`block` / `record`
        action: RuleAction,
        /// 请求被拒了吗。**拦截档 + 规则是拦**两者同时成立才会
        blocked: bool,
        /// 在工具结果里，而不是调用方自己打的字
        in_tool_result: bool,
        /// 命中处前后的一小段，**已截断**
        excerpt: String,
        at_ms: u64,
    },
    /// 模型这一次回答的正文超过了输出长度上限。**一个请求最多一条**，在超的那一刻报。
    OutputLimited {
        id: u64,
        provider: String,
        /// 上限，按字符数
        max_chars: u64,
        /// 超的那一刻数到了多少
        seen_chars: u64,
        /// 切断了吗：流从那一帧起不再发、整包整份不发。`false` = 观察档，只记录
        cut: bool,
        at_ms: u64,
    },
    /// 一个请求发出前，按出站脱敏的规则找到了东西。
    ///
    /// **观察档和拦截档报的是同一条**，差别只在 `replaced`：观察档只记录，
    /// 请求原样发出；拦截档已经把它们换成了占位符。以前两档是两条事件、
    /// 按两套规格找，于是同一个请求观察时报「检测到」，切到拦截后一处不换。
    ///
    /// **看不见的安全功能会被用户关掉** —— 他们会怀疑是脱敏搞坏了功能 ——
    /// 所以换了什么要说得出来，但一律打码。
    SecretsFound {
        id: u64,
        /// 这时要发往的上游（故障转移之前的首选）
        provider: String,
        /// 已经换成占位符了吗。`false` = 观察档，只记录
        replaced: bool,
        items: Vec<SecretItem>,
        at_ms: u64,
    },
    /// token 端点换发了新的 refresh token。
    ///
    /// **服务器换发新的那一刻，旧的已经在服务端作废了** —— 所以「不写回
    /// config.yaml」不是保守选项，它保证了配置文件从那一秒起就是坏的，
    /// 只是症状延迟到下一次重启（那家上游突然全是 401，而那时没人会
    /// 想到是几天前的一次轮换）。
    ///
    /// 所以默认写回，而 `persisted` 说的就是那一步成没成：
    ///
    /// - `true`：文件已经改好了，这条只是**告知** —— 用户的编辑器会弹
    ///   「文件已在磁盘上更改」，他该知道是谁改的。
    /// - `false`：**要一直挂着**。重启之前不处理的话，这家上游就废了。
    CredentialRotated {
        id: u64,
        provider: String,
        /// 写回 config.yaml 成功了吗
        persisted: bool,
        /// 人话。成功时说写到哪儿了，失败时说卡在哪一步。**不含 token**
        detail: Msg,
        at_ms: u64,
    },
    /// OAuth 凭据失效了：refresh token 过期、被用过或者被吊销。
    ///
    /// **重试没有用，只有重新登录能恢复**，而在那之前这家上游的请求全部失败，所以要
    /// 让人知道。只在失效的那一刻报一次；重新登录之后再失效，会重新报。
    CredentialExpired {
        id: u64,
        provider: String,
        /// 失效的原因，**已打码**
        detail: Msg,
        at_ms: u64,
    },
    /// 一次账号登录结束了：成功、失败、过期或者取消。
    ///
    /// 登录是在浏览器里完成的，**界面等的就是这一条**：收到它就能切回前台、说明结果，
    /// 不用一直问。
    LoginFinished {
        id: u64,
        /// 发起登录时拿到的 ID
        login: String,
        /// `done` / `failed` / `expired` / `cancelled`
        status: LoginStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<Msg>,
        at_ms: u64,
    },
    /// 这次请求做了方言互转（M6+）。
    ///
    /// **`dropped` 非空时必须让用户看见**：`thinking` 在 OpenAI chat
    /// 方言里没有对应物，我们只能丢 —— 但悄悄丢掉的话，用户会发现
    /// 「扩展思考开了却没生效」而完全不知道从哪儿查起。
    Translated {
        id: u64,
        provider: String,
        from: Dialect,
        to: Dialect,
        /// 丢掉的字段，按它在请求体里的位置写：`thinking`、`top_k`、
        /// `messages.content.thinking`、`messages.content.image.source.file` …
        dropped: Vec<String>,
        at_ms: u64,
    },
    /// 上游返回的响应里有一个可疑的工具调用。
    ///
    /// **这是网关位置独有的能力**：只有我们同时知道「这个调用长什么样」
    /// 和「它来自哪个上游」。客户端弹批准提示的同一瞬间弹一条通知，用户
    /// 的判断质量完全不一样 —— 人类批准工具调用时的审查很弱，尤其在一个
    /// 长任务的第几十次批准时。
    ToolCallFlagged {
        id: u64,
        provider: String,
        /// 哪个工具。「一个 bash 调用」和「一个 Read 调用」是两件事
        tool: String,
        /// 内置规则的 id，或者自定义规则的名字
        rule: String,
        /// 自定义规则
        custom: bool,
        /// 为什么值得看一眼（英文）。自定义规则是空的，名字就是说明
        why: String,
        /// 命中的那一小段，**已截断**
        excerpt: String,
        /// 这条规则在拦截档下做什么：`cut` / `record`
        action: RuleAction,
        /// 真的切断了流吗。**拦截档 + 规则是切断**两者同时成立才会
        blocked: bool,
        at_ms: u64,
    },
    /// 这次请求花了多少钱 —— **在它跑完之后一小会儿才知道**。
    ///
    /// 价钱不在数据面的职责里：网关知道用了多少 token，而单价在存储层
    /// 落库时才查价目表算出来。所以它是一条独立事件，而不是
    /// `RequestFinished` 上的一个字段 —— 后者会要求网关依赖价目表，把
    /// 「转发」挂到「计价」下面。
    ///
    /// 没有这条的话，界面想知道价钱就只能在请求结束之后回库里再查一遍。
    ///
    /// 三个值都可能是「算不出来」：上游没报用量、模型不在价目表里、
    /// 或者那家是订阅计费 —— **那都不是零**。
    RequestPriced {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_micros: Option<i64>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        cost_estimated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_saved_micros: Option<i64>,
        at_ms: u64,
    },
    /// 某家上游的熔断器开了或者合上了。
    ///
    /// **这是少数几个不挂在任何一次请求上的状态变化。**熔断是攒够三次
    /// 连续失败之后的判断，而恢复更彻底 —— 「冷却到点了」纯粹是时间
    /// 走到，没有任何调用触发它。
    ///
    /// 没有这条事件，界面想知道「哪家被熔断了」就只能轮询 `/overview`，
    /// 而那是在一条本来完全空闲的连接上每两秒问一次同样的问题。
    HealthChanged {
        id: u64,
        provider: String,
        /// `open` = 熔断中，不进候选链；`closed` = 可以用
        state: BreakerState,
        at_ms: u64,
    },
    /// 某家上游的模型清单开始获取了，或者获取完了（列出来了、没列出来、
    /// 没问到）。
    ///
    /// **后台获取不挂在任何一次调用上**：启动时、每天一次、改了地址或凭据
    /// 之后，core 自己去问。没有这条事件，界面在启动那一刻读到的「还没
    /// 获取」会一直挂着，直到别的什么事让它重读一次概览。
    ModelsChanged {
        id: u64,
        provider: String,
        at_ms: u64,
    },
    /// 某个代理通不通变了（只在变化那一刻发一次）。
    ///
    /// **没有它，代理挂了看起来就是「好几家上游同时不通」** —— 而那两件事
    /// 要做的处理完全不同。转发失败时顺手检一次那个代理，不做定时探测。
    ProxyChanged {
        id: u64,
        proxy: String,
        /// `unreachable` = 刚刚检查不通；`reachable` = 又通了
        state: ProxyState,
        /// 不通时卡在建连的哪一步。**和原因分开** —— 「卡在到代理的 TCP
        /// 握手」要去改的地方和「代理拒绝了用户名和密码」完全不同，而把
        /// 两句话拼成一个字符串之后，界面就只能整句照搬
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failed: Option<L1Stage>,
        /// 不通的原因，已脱敏。通了没有
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<Msg>,
        at_ms: u64,
    },
    /// 上游拒绝了我们的凭据（401/403），或者重新接受了。
    ///
    /// **熔断器看不见这件事**：4xx 不算失败，所以一个凭据坏掉的上游永远不会
    /// 被熔断，也就永远不会有 `HealthChanged`。而它要用户去改配置。
    AuthChanged {
        id: u64,
        provider: String,
        /// `rejected` = 上游拒绝了凭据；`accepted` = 又能用了
        state: AuthState,
        /// 被拒时上游给的状态码
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        at_ms: u64,
    },
    /// 一家上游的订阅额度：**这一家此刻的全部窗口**，收到就整个换掉，不和之前的合并。
    ///
    /// 大多来自响应头，**零成本**：不发额外请求，顺着真实流量白捡。按量付费的账号没有
    /// 这些头，那时这个事件根本不会出现 —— 而不是报一个「用了 0%」。GLM Coding Plan 和
    /// ChatGPT 账号的额度也会去问账号的接口。
    ///
    /// **`windows` 是空的：这一家的额度撤下了**（GLM 的 key 被判定没有套餐）。之前报过的
    /// 窗口都不再作数，`/quota` 里也没有它了 —— 那一格收起来，不是「用了 0%」。
    QuotaSeen {
        id: u64,
        provider: String,
        windows: Vec<QuotaWindow>,
        at_ms: u64,
    },
    /// 一个订阅额度窗口用完了，这家上游在窗口重置之前不再接受请求。
    ///
    /// **只在用完的那一刻报一次。**之后同一个窗口里的请求照样被拒，但同一句话说第二遍，
    /// 只会让人学会忽略通知。窗口重置、额度恢复之后再用完，会重新报。
    QuotaExhausted {
        id: u64,
        provider: String,
        /// 和 `QuotaWindow.window` 同一个词表
        window: String,
        /// 什么时候重置（见 `QuotaWindow::resets_at_ms`）。上游没说就没有
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resets_at_ms: Option<u64>,
        at_ms: u64,
    },
    /// 数据面换了监听地址，或者没换成（旧的还在服务）。
    ///
    /// **和 `ConfigReloaded` 是两件事。**配置换进去之后监听器才开始换，
    /// 新地址绑不绑得上要再过一会儿才知道 —— 界面只听配置那一条的话，
    /// 读到的永远是换之前的地址。
    ListenChanged {
        id: u64,
        /// 此刻在听的那个地址，同 `Status::gateway_addr`
        #[serde(default, skip_serializing_if = "Option::is_none")]
        addr: Option<String>,
        /// 没换成的原因。换成了就没有
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<Msg>,
        at_ms: u64,
    },
    /// 配置换了一份新的进去，已经生效。
    ///
    /// **界面靠它知道自己手里那份过期了。**没有它，用户在编辑器里改完
    /// 文件，界面上还显示着旧的 —— 而他分不清是我们没生效还是界面没刷新。
    ConfigReloaded {
        id: u64,
        /// 内容版本号，和 `PATCH /config` 的 `base_version` 是同一个
        version: String,
        /// `ui` / `cli` / `external` / `rollback` / `rotation`
        origin: ConfigOrigin,
        at_ms: u64,
    },
    /// 新配置没过关，**旧的还在服务**。
    ///
    /// 桌面工具不能因为一个笔误就断线，所以这不是崩溃，是一条要展示给
    /// 人看的信息 —— 托盘变黄、界面标红、定位到那一行。
    ConfigRejected {
        id: u64,
        /// `syntax`（YAML 写坏了）/ `schema`（字段名或取值不对）/
        /// `semantics`（单看每个字段都对，合起来不成立）
        stage: ConfigStage,
        message: Msg,
        /// 1 起。语义错误没有，那时硬指一行只会误导
        line: Option<usize>,
        /// 出错那一行的原文，**已脱敏**
        excerpt: Option<String>,
        /// 这一版是谁写的：`ui` / `cli` / `external` / `rollback` / `rotation`。
        ///
        /// **界面靠它区分「用户在编辑器里写错了」和「界面自己刚写坏了」** ——
        /// 前者要提醒，后者是保存失败，那条路自己会报。
        origin: ConfigOrigin,
        at_ms: u64,
    },
    /// 客户端的辅助请求被本地应答了，一个字节都没发给上游。
    ///
    /// **单独一个事件，不复用 RequestFinished。**它的成本是 0、延迟是
    /// 0，混进请求总数和延迟统计里会让那两个数字都变得没意义 —— 而
    /// 「本地应答了 N 次」本身是个正向数字，值得单独让用户看见。
    LocallyAnswered {
        id: u64,
        client: String,
        /// 请求头透出来的旁证，同 `RequestStarted`
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_hint: Option<String>,
        /// 请求从哪台机器来：**这条连接对面的地址**，不可伪造。本机（回环）
        /// 来的不记 —— 那是绝大多数请求，写上只是噪音；局域网来的才值得
        /// 说一句「来自 192.168.1.23」。几台机器共用一把密钥时，只有它分得开
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peer: Option<String>,
        /// 请求带的那把网关密钥打码后的样子（`tw-re…wb4e`）。**记的是请求那一刻
        /// 用的那把** —— 密钥更换过之后，老记录上的尾巴照样对得上
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key_masked: Option<String>,

        /// 哪一类辅助请求，和 `ProbeView.id` 同一个词表。字段叫 `probe` 而
        /// 不是 `kind` —— 那个名字已经被枚举的 tag 占了
        probe: ProbeClass,
        at_ms: u64,
    },
    /// 这个订阅者跟不上，事件流丢了它 `count` 条事件。
    ///
    /// **只发给掉队的那一个**，不进总线：别的订阅者什么都没丢。收到它就说明
    /// 只靠增量维护的东西（进行中的请求、列表里每一行的状态）从这一刻起不可信，
    /// 要整体对一次账：重读 `/in-flight`、最近的历史和概览。
    ///
    /// 以前丢了只记一行日志。那时事件只喂几行实时日志，少几行无所谓；现在
    /// 「进行中」的计数和每一行的状态都靠事件，丢一个结局，那一行就永远停在
    /// 「进行中」。
    EventsDropped { id: u64, count: u64, at_ms: u64 },
}

slug_enum! {
    /// 尝试链里一跳的结果。
    pub enum AttemptOutcome {
        /// 这一跳接下了请求，尝试链到此为止。上游回的是 4xx 也算 —— 请求本身有
        /// 问题，换一个上游也一样被拒
        Served = "served",
        /// 上游返回 5xx 或 429，换下一个上游
        Status = "status",
        /// 没有收到响应（超时、无法连接），或者这一跳没有发出去
        Error = "error",
    }
}

/// 尝试链里的一跳。
///
/// **失败的原因要留着** —— 一条说「试过 A → B → C」的链，和一条还说清
/// 每一跳为什么失败的链，排查价值差得远。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct AttemptView {
    pub provider: String,
    /// WebSocket 的那一跳是一次握手：上游同意升级（101）是 `served`，回了别的
    /// 状态码是 `status`，连不上是 `error`。
    pub outcome: AttemptOutcome,
    /// 上游返回的状态码。`error` 时没有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// `error` 时的说明。和这一跳报给客户端的那条错误是同一句
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Msg>,
    pub ms: u64,
}

/// 一次请求的路由决策。**详情抽屉的 Routing 那一页吃它。**
///
/// 字段就是 [`Event::RequestRouted`] 里的那些（计费方式记在那一行上）。路由还没
/// 报出结论请求就结束了的（上游应答之前客户端就走了）也有：开始时就知道的那几项，
/// 尝试链是空的。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RoutingView {
    /// 走的哪条路由：密钥指定的那条，没指定的是默认路由
    pub route: String,
    /// 决定去向的那条规则：第一条命中的转发或拒绝
    pub rule: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// 附加了参数改写的规则，按求值的顺序（见 `RequestRouted::rewritten_by`）
    pub rewritten_by: Vec<String>,
    /// 选定上游之后才判断的规则拒绝了它：是哪一条
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied_by: Option<String>,
    pub attempts: Vec<AttemptView>,
}

/// 一个订阅额度窗口。**每个字段都直接来自上游**：响应头，或者账号的额度接口。
///
/// 我们自己推断的东西不放进这个结构 —— 界面上必须能区分「上游说的」和
/// 「我们猜的」，而混在一个类型里就区分不了了。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct QuotaWindow {
    /// `5h` / `7d`（Anthropic）/ `weekly`（Codex、GLM）
    pub window: String,
    pub used_percent: f64,
    /// 什么时候重置，Unix 毫秒。**是时刻，不是「还有多少秒」**：上游报的秒数
    /// 只在收到响应那一刻成立，存下来原样给出去，界面上就是一个不会走的倒计时。
    /// 上游没说就没有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// 这个窗口的积分。**只有 GLM Coding Plan 积分制套餐的窗口有**（额度接口里的
    /// `CREDIT_LIMIT`）；按 token 算的老套餐、Anthropic、Codex 的窗口都没有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<QuotaCredits>,
}

/// 积分制套餐一个额度窗口的积分：GLM Coding Plan 额度接口里 `CREDIT_LIMIT` 那一项的
/// 总额（`usage`）、已用（`currentValue`）、剩余（`remaining`）。**只有这一种**，别的
/// 窗口不带它。三个数都是上游给的原数。
///
/// **剩余不是总额减已用算出来的**：上游给的三个数不一定对得上（实测总额 2000、已用 23、
/// 剩余 1976），界面要显示剩余就显示它说的剩余。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct QuotaCredits {
    pub total: f64,
    pub used: f64,
    pub remaining: f64,
}

/// 一次调用的用量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct UsageView {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// 缓存写用的是 1 小时 TTL 吗。**差价接近一倍**
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cache_1h: bool,
}

impl Event {
    pub fn id(&self) -> u64 {
        match self {
            Event::RequestStarted { id, .. }
            | Event::RequestHeaders { id, .. }
            | Event::RequestFinished { id, .. }
            | Event::RequestFailed { id, .. }
            | Event::RequestCancelled { id, .. }
            | Event::LocallyAnswered { id, .. }
            | Event::EventsDropped { id, .. }
            | Event::ConfigReloaded { id, .. }
            | Event::ListenChanged { id, .. }
            | Event::ConfigRejected { id, .. }
            | Event::QuotaSeen { id, .. }
            | Event::QuotaExhausted { id, .. }
            | Event::SecretsFound { id, .. }
            | Event::HiddenTextFound { id, .. }
            | Event::ContentMatched { id, .. }
            | Event::OutputLimited { id, .. }
            | Event::RequestPriced { id, .. }
            | Event::HealthChanged { id, .. }
            | Event::ModelsChanged { id, .. }
            | Event::ProxyChanged { id, .. }
            | Event::AuthChanged { id, .. }
            | Event::ToolCallFlagged { id, .. }
            | Event::Translated { id, .. }
            | Event::CredentialRotated { id, .. }
            | Event::CredentialExpired { id, .. }
            | Event::LoginFinished { id, .. }
            | Event::RequestRouted { id, .. } => *id,
        }
    }
}

/// 此刻的实时读数（`GET /live`）：在跑的请求，和最近跑完的请求生成得多快。
///
/// **按事件数出来的，core 里只数一份。**以前菜单栏自己听事件流、自己记在跑的和
/// 跑完的：半路才开始听的那一段要拿 `/in-flight` 对账，丢过事件要重对，而这些
/// core 的事件总线本来就在记。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct LiveView {
    /// 开始了、还没有结局的请求，开始得早的在前。和 `/in-flight` 是同一批
    pub running: Vec<RunningView>,
    /// 最近一分钟跑完的请求平均每秒生成多少 token：**每个请求的输出除以它生成用的
    /// 时间**（总耗时减去首字节），按 token 加权。这一分钟里没有跑完的是空，不是 0
    pub tokens_per_sec: Option<u32>,
}

/// 一个在跑的请求。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RunningView {
    pub id: u64,
    /// 网关密钥的名字
    pub client: String,
    /// 按请求头推测的应用。**可以伪造**，只用来显示
    pub client_hint: Option<String>,
    pub model: String,
    /// 开始时的首选上游
    pub provider: String,
    /// 开始的时刻，**core 的时钟**
    pub at_ms: u64,
    /// 已经跑了多久，毫秒，**core 算的**（按它自己的单调时钟）。界面拿自己的时钟
    /// 去减 `at_ms` 的话，连的是另一台机器上的 core 时两边的时钟不一定对得上；
    /// 问完之后往上走的那一截由界面自己数
    pub elapsed_ms: u64,
    /// 归到哪一次会话，同 `RequestStarted::session`
    pub session: Option<String>,
    /// 走的哪条路由、哪条规则决定的去向、经过哪个策略组 —— 开始时就知道
    pub route: String,
    pub rule: String,
    pub group: Option<String>,
    /// 实际接下它的上游：尝试链里接下了的那一跳。**路由报出结论之前没有**
    /// （还在等上游的响应头，或者正在故障转移）
    pub upstream: Option<String>,
}

/// 此刻还在跑的请求（`GET /in-flight`）：每一个到目前为止的事件，和 core 此刻的
/// 时钟。
///
/// **给半路才来听事件流的一方重建现状用。**事件流只送订阅之后发生的事；一个在那
/// 之前就开始、此刻还没结束的请求，它的开始、响应头、路由都早就发过了。把每个
/// 请求的 `events` 按顺序喂给自己处理事件的那段代码，就和从头听起一样。
///
/// **和 `Status::in_flight` 不是一个数。**那个从连接进到数据面就算，排队等名额的、
/// 鉴权没过的都在里面 —— 它回答的是「现在重启会掐断几个连接」。这里只有发过开始
/// 事件的请求，和事件流里说的是同一批，也和 `LiveView::running` 是同一批。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct InFlight {
    /// core 此刻的时钟，Unix 毫秒。**算「跑了多久」用它减开始事件的 `at_ms`**，
    /// 两个都是 core 的时钟；界面自己的时钟和远程 core 的不一定对得上
    pub now_ms: u64,
    /// 开始了、还没有结局的请求，开始得早的在前
    pub requests: Vec<InFlightRequest>,
}

/// 一个还在跑的请求到目前为止的事件。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct InFlightRequest {
    pub id: u64,
    /// 关于它的事件，**照事件流上的样子、按发生的先后**：第一条是 `RequestStarted`，
    /// 之后是到目前为止发生了的 —— 响应头、路由、格式转换、防护的记录
    /// （`RequestHeaders`、`RequestRouted`、`Translated`、`SecretsFound`、
    /// `HiddenTextFound`、`ContentMatched`、`OutputLimited`、`ToolCallFlagged`）。
    /// 说的是上游现状的（`QuotaSeen`）不在里面：那是 `/quota` 的事
    pub events: Vec<Event>,
}

/// 配置文件没通过校验的那一次。字段和 [`Event::ConfigRejected`] 一样。
///
/// **只记外部改动**（在编辑器里改的、命令行写的）：界面自己写坏的根本没落盘，
/// 那是一次保存失败，保存那条路当场就说了。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ConfigRejection {
    pub stage: ConfigStage,
    pub message: Msg,
    /// 1 起。语义错误没有
    pub line: Option<usize>,
    /// 出错那一行的原文，**已脱敏**
    pub excerpt: Option<String>,
    pub at_ms: u64,
}

/// 界面要显示的配置概览。
///
/// **不是配置文件本身**：密钥一律只给来源描述，不给值（统一脱敏）。
/// 界面需要的是「有哪些上游、规则怎么写的、谁健康」，不是那份 YAML。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Overview {
    /// 配置文件现在的版本号。和 `GET /config` 的 `version`、改配置时带的
    /// `base_version` 是同一个。
    ///
    /// **跟着概览一起给。**界面上能改的每一格都画自这份概览，改的时候要带上
    /// 它。分开读的话，界面在 core 起来之前挂上、那一次读失败，就一直拿不到
    /// —— 所有写入都卡在「还没读到版本号」上，等多久都不会好。
    pub config_version: String,
    pub providers: Vec<ProviderView>,
    /// 配置里定义过的代理。**界面上换代理要从这里选** —— 让用户
    /// 手打一个名字，打错了就是一次静默的「配了没生效」
    pub proxies: Vec<ProxyView>,
    pub routes: Vec<RouteView>,
    pub groups: Vec<GroupView>,
    pub clients: Vec<ClientView>,
    pub listen: ListenView,
    /// 两项防护各在哪一档。
    ///
    /// **界面要能配它们，而不只是显示。**三态的整个设计前提是「出厂停在
    /// 观察态，用户看到证据之后自己决定要不要切到拦截」，一个切不了的开关
    /// 让那个设计不成立。规则在 `/security` 里。
    pub security: SecurityView,
    /// 没绑路由的密钥走哪条
    pub default_route: String,
    /// 客户端自己发的辅助请求怎么处理
    pub client_probes: Vec<ProbeView>,
    /// 日志留多久
    pub retention: RetentionView,
    /// 自定义价目表。默认价目表不在这里 —— 它的状态看 `/pricing`
    pub price_sheets: Vec<PriceSheetView>,
}

/// 一张自定义价目表。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PriceSheetView {
    pub name: String,
    pub multiplier: f64,
    /// 单独覆盖了几个模型
    pub overrides: usize,
    /// 哪些上游选了它。**删之前要知道**，改名时它们会跟着改
    pub used_by: Vec<String>,
}

/// 一个出站代理。
///
/// **密码不在这里。**`ProxyAuth.pass` 和上游的 key 是同一类东西 ——
/// 这个视图会进日志、进诊断包、进用户贴出来的截图。有没有认证是要
/// 显示的，认证内容不是。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProxyView {
    pub name: String,
    /// `socks5h` / `socks5` / `http` / `https`
    pub kind: ProxyKind,
    pub addr: String,
    /// 有没有认证。**用户名和密码都不在这里** —— 用户名是凭据的一半，
    /// 而这个视图会进日志、进诊断包、进用户贴出来的截图
    pub has_auth: bool,
    /// 哪些上游在用它。删之前要知道，改名时它们会跟着改
    pub used_by: Vec<String>,
    /// 网关发现它不通了：经它转发的请求连不上之后检过一次，卡在哪一步、为什么。
    /// 之后经它的请求成功了、或者再检一次通了，就又是空的。
    ///
    /// **不定时探测**（见 `ProxyChanged`），所以空的意思是「没发现问题」，不是
    /// 「刚测过是通的」。和 `ProxyChanged` 说的是同一件事：一个是现状，一个是变化
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unreachable: Option<ProxyFault>,
}

/// 网关发现一个代理不通时，检出来的样子。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProxyFault {
    /// 卡在哪一步。说不出来的没有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<L1Stage>,
    pub detail: Msg,
    /// 什么时候检的
    pub at_ms: u64,
}

/// 一类客户端辅助请求的处置。
///
/// **这一段以前在界面上完全不存在，而它的缺席是连锁的**：路由条件
/// `when.intent` 只有在对应那一类被配成 `route` 时才可能命中，所以
/// 界面上那些写了 `intent` 的规则永远不会生效，而用户无从知道为什么。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProbeView {
    /// `health_check` / `warmup` / `titling` / `topic_detect` / `suggestion`
    pub id: ProbeClass,
    /// `intercept` / `route` / `passthrough`
    pub mode: ProbeMode,
}

/// 日志留多久。两个期限分开，因为正文和记录行的代价差三个数量级 ——
/// 一条正文几十 KB，一行记录几百字节。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RetentionView {
    /// 请求和响应的正文留几天
    pub body_days: u64,
    /// 一行记录留几天
    pub row_days: u64,
    /// 正文总共最多占多少字节
    pub body_max_bytes: u64,
    /// 正文现在实际占了多少。**不是配置，是现状** —— 没有它，
    /// 「2 GB 上限」是个用户无从判断松紧的数字
    pub body_bytes_now: u64,
}

/// 每项防护各在哪一档：`off` / `observe` / `enforce`。
///
/// **「拦截」在各项上做的事不一样**：脱敏是替换成占位符，工具调用审查和输出长度
/// 是切断响应，藏匿字符和内容过滤是拒绝请求。
/// 规则和日志在 [`SecurityDetail`] 和 `/security/events` 里，不塞进概览。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SecurityView {
    pub redact: GuardMode,
    pub inspect_tools: GuardMode,
    pub hidden_text: GuardMode,
    pub content: GuardMode,
    pub output_limit: GuardMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProviderView {
    pub name: String,
    /// 已脱敏。**编辑时不要原样写回** —— 地址里带了凭据的话，写回去的
    /// 是打过码的那一份。`base_url_masked` 告诉界面这一点
    pub base_url: String,
    /// 地址里有被打码的部分（userinfo 之类）
    pub base_url_masked: bool,
    /// API 密钥：打过码的值，或者环境变量名。没有密钥是空
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<SecretView>,
    /// 密钥放在哪个请求头里发：`x-api-key` / `authorization` / `x-goog-api-key`
    pub auth_header: String,
    /// 其余请求头，按配置里的顺序
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<HeaderView>,
    /// OAuth 的 token 端点和 client id。**refresh token 和 client secret 永远不出这个进程**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthView>,
    /// 实际生效的协议：`anthropic` / `openai-chat` / `openai-responses` /
    /// `gemini`。猜不出来时为空
    pub protocol: Option<Protocol>,
    /// 协议是配置里写明的，还是按地址推断的
    pub protocol_explicit: bool,
    /// `direct` / `system` / 代理名
    pub proxy: String,
    /// `fail` / `direct`
    pub on_proxy_fail: OnProxyFail,
    /// 服务不提供模型列表时用的手动清单
    pub models: Vec<String>,
    /// 启用范围：只用这些模型（ID 或 glob）。空 = 它提供的全部
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_only: Option<Vec<String>>,
    /// 模型清单从哪儿来：`discovered`（上游列出的）/ `manual`（手动清单）/
    /// `none`（不知道它有什么）
    pub model_source: ModelSource,
    /// 最近一次向上游获取清单的结果
    pub model_status: ModelListStatus,
    /// 正在获取。上一次的结果照常有效
    pub model_fetching: bool,
    /// 最近一次获取的时间。还没获取过是空
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_checked_at_ms: Option<u64>,
    /// `no_list` / `failed` 的原因
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_error: Option<Msg>,
    /// 现在能服务的模型数，已按启用范围过滤。停用时是 0
    pub model_count: usize,
    /// 停用：不参与路由，模型不出现在 `/v1/models` 里
    pub disabled: bool,
    /// `ok` / `open`（熔断中）
    pub health: Health,
    /// 上游拒绝了凭据：最近一次得到答复的请求回的是这个状态码（401 / 403）。
    /// 没被拒是空的，被拒之后有请求成功了也是空的。
    ///
    /// **熔断看不见这件事**：4xx 不算失败，换一家也一样被拒，所以一家凭据坏掉的
    /// 上游永远不会熔断。和 `AuthChanged` 说的是同一件事：一个是现状，一个是变化
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_rejected: Option<u16>,
    /// token 端点换发的新凭据**没能写回配置**的原因。这时新凭据只在内存里，旧的
    /// 已经作废：网关一重启，这家上游就要重新登录。写回成功就没有了
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writeback_failed: Option<Msg>,
    /// 计费方式：`per-token`（按价目表算，订阅账号也是）/ `free`（记 $0）
    pub billing: Billing,
    /// 谁在引用它。**删之前要知道**，改名时它们会跟着改
    pub references: Vec<ReferenceView>,
    /// 选的价目表。空 = 默认价目表
    pub pricing: Option<String>,
}

/// 一个可能是密钥的值给界面看的样子。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SecretView {
    /// 打过码的值。带 `${NAME}` 的值原样给 —— 它写的是从哪个环境变量读
    pub display: String,
    /// 整个值恰好是一个 `${NAME}` 时的变量名。变量名不是秘密，编辑时要回填
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
}

/// 一行请求头给界面看的样子。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct HeaderView {
    pub name: String,
    /// **可能是密钥的值是打过码的**；公开的头（`anthropic-version` 之类）、
    /// 只由环境变量和占位符组成的值原样给
    pub value: String,
    /// 值打过码。编辑时这一行不回填，留空表示保持原值
    pub masked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct OAuthView {
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// access token 什么时候过期，RFC 3339。不知道就没有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// 最近一次刷新失败的原因。**已打码**；没失败过、或者已经恢复就没有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Msg>,
    /// 凭据已经失效，只有重新登录能恢复
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_login: bool,
    /// 登的是哪个账号。**ChatGPT 账号上游才有**，令牌里一项都读不出来时没有。
    ///
    /// Z.ai / BigModel 的账号上游没有 `oauth` 这一节，也就没有它：那类登录的终点是一把
    /// 普通 API key，key 里读不出是谁，登录时读到的账号名只在那一次登录的结果里
    /// （[`ZaiLoginStatus::account`]），不随 key 保存
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountView>,
}

/// 一个账号上游登的是哪个账号。
///
/// **从这份凭据自己的 access token 里读，不联网**：ChatGPT 的 access token 是 JWT，套餐
/// 在 `https://api.openai.com/auth` 里，邮箱在 `https://api.openai.com/profile` 里。登录
/// 和每一次刷新都换来一个新的、并且写回配置，所以它跟着凭据走：套餐变了，下一次换
/// token 之后这里就是新的；凭据失效之后，这里仍是最后登着的那个账号。
///
/// 账户 ID、用户 ID 同在令牌里，**不往外带**：界面认账号靠邮箱，那两个读不出是谁，
/// 而它们一旦出去就会进日志
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct AccountView {
    /// 令牌里没写就没有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// 令牌里没写就没有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<ChatgptPlan>,
}

/// 配置里引用了某个上游的一处。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReferenceView {
    /// 路由规则的去向（`to`）
    RuleTarget { route: String, rule: String },
    /// 路由规则的条件（`provider_would_be`）
    RuleCondition { route: String, rule: String },
    /// 策略组的成员
    Group { group: String },
}

/// 一条规则。
///
/// **是全文，不是摘要** —— 编辑对话框靠它回填：条件、去向、拒绝原因、
/// 参数改写、安全要求，交回来的 [`RuleInput`] 是同一套写法。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RuleView {
    pub name: String,
    /// `when` 里写了的条件，按固定顺序。空 = 兜底
    pub conditions: Vec<ConditionView>,
    /// 去向：上游名或组名。拒绝的规则和只附加改写的规则没有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// 命中就拒绝。值是返回给客户端的原因
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<String>,
    /// 参数改写
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<RuleRewrite>,
    /// 没有条件，匹配全部请求
    pub catch_all: bool,
    /// 在选定上游之后才判断（条件里有 `provider_would_be`）
    pub phase_two: bool,
    /// 它的转发或拒绝不会被采用：前面已经有一条匹配全部请求的转发或拒绝。
    /// 它附加的改写照常生效
    pub shadowed: bool,
}

/// 规则里的参数改写。每一项不写就是不改。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RuleRewrite {
    /// 换一个模型。**整个 prompt cache 随之作废**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// 打开或关闭扩展思考
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
}

/// 规则里的一个条件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ConditionView {
    /// `when` 里的键：`model` / `client` / `dialect` / `input_tokens` /
    /// `max_tokens` / `tool_count` / `intent` / `provider_would_be` /
    /// `cache` / `tools` / `image` / `thinking` / `stream`
    pub field: ConditionField,
    /// 写的值。`intent` 和 `provider_would_be` 可以写多个，满足其一即可；
    /// 布尔条件是 `true` / `false`；数量条件是比较式（`>200k`）
    pub values: Vec<String>,
}

/// 一条路由 —— 一组规则，加上它分给了谁。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RouteView {
    pub name: String,
    /// 没绑路由的密钥走的就是这条
    pub default: bool,
    /// 网关按配置补出来的默认路由：配置文件里没有它。**编辑并保存即写入配置**
    pub builtin: bool,
    /// 有一条匹配全部请求的转发或拒绝。没有的话，哪条规则都没命中的请求会失败
    pub has_catch_all: bool,
    /// **显式绑了这条路由的密钥。**默认路由这里通常是空的 —— 走它的人
    /// 是「没绑」，不是「绑了它」，而把所有密钥列进来会让人以为那是
    /// 一次次显式的选择。
    pub clients: Vec<String>,
    pub rules: Vec<RuleView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct GroupView {
    pub name: String,
    /// 内置的「全部上游」：成员是全部上游，按上游列表的顺序。不能编辑、不能删除
    pub builtin: bool,
    /// 配置里写的 `type`
    pub kind: GroupKind,
    /// 同一次会话固定走同一家。**这一项直接决定账单**
    pub session_affinity: bool,
    /// `select` 组当前选中谁。
    ///
    /// **界面要能切它** —— 这个策略本身就是「UI 上点选或托盘里切」，
    /// 而切不了的话它等于一个只能改 YAML 才能用的功能。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
    pub providers: Vec<String>,
    /// 这个组**按现在的配置**会不会让 prompt cache 不稳定（开着会话粘滞的
    /// 轮询组不会）。**要在界面上直说** —— 它决定了用户的账单。
    pub hurts_cache: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ClientView {
    pub name: String,
    /// `GET /keys` 给明文 —— 密钥页要把它原样显示出来、给出复制按钮。
    /// `GET /overview` 里是脱敏的：概览到处都在读，它用不着密钥的值
    pub key: String,
    pub max_concurrent: Option<usize>,
    /// 绑的那条路由。`None` = 走默认路由
    pub route: Option<String>,
    /// 这把密钥能看到哪些模型。三态：不写 / 写非空 / 写 `[]`（一个都不给）
    pub allow: Option<Vec<String>>,
    /// 为哪个客户端生成的（`claude-code` / `codex` …）。取消接管之后仍然记着
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// 停用之后，用这把密钥的请求一律拒绝
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    /// 没有为自己生成密钥的客户端用的就是它。**删不得**
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default: bool,
    /// 最后一次被用在什么时候。**按密钥算，不是按客户端自报的标识** ——
    /// 那个可以伪造。从来没被用过时没有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_ms: Option<u64>,
}

/// 新建或保存一把网关密钥（`POST /keys`、`PUT /keys/{name}`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct KeySave {
    pub key: KeyInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
}

/// 一把密钥上用户能改的东西。**密钥的值不在里面** —— 它由 core 生成，
/// 要换就走更换（新值写进已接管的客户端，由桌面端做）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct KeyInput {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<usize>,
    /// `None` = 走默认路由
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    /// 三态：不写 / 写非空 / 写 `[]`（一个都不给）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

/// 换哪把密钥（`POST /keys/{name}/rotate`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct KeyRotate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
}

/// 换完之后的结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct KeyRotated {
    pub version: String,
    /// 新的密钥值。**只在这里给一次**：正被接管的客户端的配置要换成它，由桌面端写
    pub key: String,
}

/// 保存监听设置（`PUT /listen`）。
///
/// **三项一起存**，而不是三个补丁：从「仅本机」换到「局域网」时网卡和
/// 端口往往一起改，分开存的话中间那一版监听在一个用户没选过的地址上。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ListenSave {
    /// 同配置里的 `listen.gateway.bind`：`loopback` / `all` / 网卡名 / 地址
    pub bind: String,
    pub port: u16,
    /// 放行网段。**只在监听超出本机时有意义**；空 = 除本机外谁都连不上。
    /// 和默认名单一样时配置里不写这一项
    pub allow_from: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
}

/// 设默认密钥（`PUT /default_key`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct DefaultKeySave {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
}

/// 一把密钥的明文（`GET /keys/{name}/value`）。「复制」按名字取此刻配置里的
/// 值，不依赖界面手里那份列表是不是最新的。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct KeyValue {
    pub name: String,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ListenView {
    pub bind: String,
    pub port: u16,
    /// 放行网段，就是生效的那一份：配置里没写时是默认名单，空 = 除本机外
    /// 谁都连不上。本机永远放行，不在名单里
    pub allow_from: Vec<String>,
    /// 默认名单（私网段）。界面上「恢复默认」用
    pub default_allow_from: Vec<String>,
    /// 非 loopback 时为真。界面上要据此把「关闭密钥校验」置灰
    pub exposed: bool,
}

/// 模型清单的结果。**空列表不足以表达**：「上游没这个接口」「上游给了
/// 但我们没认出格式」「真的一个都没有」是三件事，塌成空列表之后 UI 只能
/// 说「这家不提供模型列表」，而那在第二种情况下是编的 —— 把我们自己的
/// 解析缺口说成了对方的特性。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelList {
    Listed {
        models: Vec<String>,
    },
    /// 上游没有这个接口。不是错误，但按模型路由那类功能对它用不了。
    NotImplemented {
        status: u16,
    },
    /// 2xx 但我们没认出形状 —— **这是我们的缺口，要报出来去修**。
    Unrecognized {
        sample: String,
    },
    Empty,
}

/// 检测一个上游的结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProviderTestResult {
    /// 地址通、凭据被接受
    pub ok: bool,
    /// 按哪种协议测的
    pub protocol: Option<Protocol>,
    pub latency_ms: u64,
    pub models: ModelList,
    /// 经由哪个代理。直连时为空
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// 失败的原因，和下一步该查什么
    pub error: Option<Msg>,
}

/// L1 测速：只握手，不发业务请求。**零成本零副作用**。
///
/// 给了名字就测那一家，不给就测所有上游。代理自己的检测走
/// `/proxies/test`，还没保存的上游走 `/providers/test`。
///
/// **不接受一个「候选 URL 列表」。** cc-switch 有那么一张表，测完还得手动
/// 点一下填进去，运行时永远只认当前保存的那一个 —— 同一个概念在一个程序
/// 里存在两次，两边不通。这里的规矩是：测的候选池就是运行时故障转移的
/// 候选池，同一份数据（架构红线）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct L1Request {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

slug_enum! {
    /// 建连的哪一步。
    pub enum L1Step {
        /// 地址或代理配置用不了，没有开始建连
        Config = "config",
        Dns = "dns",
        Tcp = "tcp",
        Tls = "tls",
        /// 代理协议的握手，含认证
        Handshake = "handshake",
    }
}

slug_enum! {
    /// 建连的那一步对着谁。
    pub enum L1Peer {
        Upstream = "upstream",
        Proxy = "proxy",
    }
}

slug_enum! {
    /// 建连时没有出现的那一步为什么没有。
    pub enum L1SkipReason {
        /// `http://` 地址没有 TLS
        PlainHttp = "plain_http",
        /// 地址已经是 IP，不需要解析
        IpAddress = "ip_address",
        /// `socks5h` 和 HTTP CONNECT 由代理解析域名
        ProxyResolves = "proxy_resolves",
    }
}

/// 建连的哪一步、对着谁。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct L1Stage {
    pub step: L1Step,
    pub peer: L1Peer,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct L1Segment {
    pub stage: L1Stage,
    pub ms: u64,
}

/// 没有出现在分段里的那一步，和原因。**不说的话，缺一段看起来就像 bug。**
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct L1Skip {
    pub stage: L1Stage,
    pub reason: L1SkipReason,
}

/// **分段是个列表而不是固定的 DNS/TCP/TLS 三段**，因为走代理时的形状本来
/// 就不同：多出代理握手，而 `socks5h` 下根本没有本地 DNS 那一段。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct L1Result {
    /// 测的是哪个上游或代理的名字 —— 回显出来，别让用户猜点的那一下测了谁
    pub target: String,
    /// 经过哪个代理，直连是 None
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    pub ok: bool,
    pub segments: Vec<L1Segment>,
    pub total_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<L1Skip>,
    /// 失败在哪一步
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<L1Stage>,
    /// 失败的原因
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Msg>,
}

/// 当前的配置文本，连同它的版本号。
///
/// **给的是原文，不是结构。**界面的文本模式直接显示它；表单模式改完
/// 之后带着 `version` 回来，那就是乐观并发的凭据。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ConfigText {
    pub path: String,
    pub text: String,
    /// `blake3:xxxxxxxxxxxx`，和 `PATCH` 的 `base_version` 是同一个
    pub version: String,
}

/// 这台机器上的一张网卡（`GET /interfaces`），**一张一行**。
///
/// **界面上「绑在哪张网卡」那个选单要的就是它。**没有它，用户只能自己
/// 去 `ifconfig` 抄一个地址填进配置文件，而填错的后果是网关起不来。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct NicView {
    /// `en0`、`lo0`、`utun3`。配置里按它存
    pub name: String,
    /// 绑这张网卡时真正监听的地址：有 IPv4 就是 IPv4
    pub addr: String,
    /// 回环地址。界面上这一档叫「仅本机」，不该混在「选一张网卡」里
    pub loopback: bool,
}

/// 改配置。
///
/// **不是「把整份新配置发过来」**，是「基于哪一版、改哪几个字段」。
/// 整份发过来的话，两个人同时改就必然有一个人的改动被悄悄吃掉 ——
/// 而那正是 cc-switch 那批 issue 的形状。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ConfigPatch {
    /// 你基于哪一版。**对不上就是 409。**不给表示「我知道我在覆盖」
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
    pub ops: Vec<PatchOp>,
}

/// 一次字段改写。
///
/// `path` 用**名字**而不是下标：`/providers/relay-cn/base_url`。
/// 下标会在用户重排上游之后指向另一个东西，而那种错误完全静默。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PatchOp {
    Replace {
        path: String,
        value: PatchValue,
    },
    /// 往块式列表末尾加一项。
    ///
    /// `item` 是这一项的 YAML 片段，不带前导的 `- `。**结构性的编辑
    /// 只有这一条路** —— 没有它，界面只能改已经存在的标量，而新建
    /// 任何东西（一把密钥、一条规则、一个代理）都不是替换。
    Append {
        path: String,
        item: String,
    },
    /// 删掉列表里的一项。
    ///
    /// `path` 指向**那一项**，按名字：`/clients/codex`。和 `Replace`
    /// 同一条纪律 —— 下标会在用户重排之后指向另一个东西，而那种错误
    /// 完全静默。
    Remove {
        path: String,
    },
    /// 把一个列表清成空的（`[]`）。
    ///
    /// **和「把这个键删掉」不是一回事**，所以它不是 `Remove` 的循环：
    /// `allow` 不写 = 跟客户端方言走，`allow: []` = 一个都不给。界面上
    /// 那是两个不同的选项。
    Clear {
        path: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(untagged)]
pub enum PatchValue {
    Str(String),
    Int(i64),
    Bool(bool),
    /// 显式的空。`null` 和空字符串是两回事
    Null,
}

// ─────────────────────────────────────────────────────────── 价目表

/// 默认价目表现在的状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PricingStatus {
    /// 这份表的数据日期。**费用旁边要标它**
    pub date: String,
    /// `builtin`（随版本内置）/ `fetched`（联网刷新过）/ `empty`
    pub source: PricingSource,
    /// 表里有多少个模型
    pub models: usize,
    /// 定期刷新开没开
    pub auto_update: bool,
    /// 最近一次刷新的时间，成功失败都算。这次启动以来没刷过就是空
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at_ms: Option<u64>,
    /// 最近一次刷新失败的原因
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Msg>,
    /// 最近 7 天里**无法计价**的请求数。
    ///
    /// 用户不会主动想起要配价格，只有「有 37 次请求无法计价」这种具体
    /// 证据才会。
    pub unpriced_recent: i64,
    /// 那些请求走的是哪个上游、哪个模型。**直接告诉他要在哪张价目表里设
    /// 什么**，按请求数从多到少
    pub unpriced_models: Vec<UnpricedModel>,
}

/// 一个无法计价的 (上游, 模型)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct UnpricedModel {
    pub provider: String,
    pub model: String,
    pub requests: i64,
}

/// 刷新了一次之后。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PricingRefreshed {
    pub status: PricingStatus,
    /// 和刷新之前比，价格变了、新增或者移除了的模型数
    pub changed: usize,
}

/// 开关定期刷新。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct AutoUpdateSave {
    pub on: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
}

/// 一个模型的单价，**每百万 tokens 的美元**，和厂商定价页上印的一样。
///
/// 查价返回的是**实际计费用的**单价：数据集里没单独定价的缓存档已经按
/// 计费规则补上。所以它可以原样作为一条覆盖价的起点。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PriceFields {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    /// 单次请求输入超过 200K tokens 之后的单价。**成对出现**；没有 = 不分档
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_above_200k: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_above_200k: Option<f64>,
}

/// 一个价格是从哪儿来的。**每一笔费用都要能追溯到它。**
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PriceSourceView {
    /// 默认价目表。`date` 是那份表的数据日期
    Default { date: String },
    /// 默认价目表 × 某张价目表的倍率
    Scaled {
        sheet: String,
        multiplier: f64,
        date: String,
    },
    /// 某张价目表单独覆盖的
    Override { sheet: String },
}

/// 新建或修改一张价目表时交过来的定义。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PriceSheetInput {
    pub name: String,
    /// 作用于默认价目表的全部单价
    #[serde(default = "one")]
    pub multiplier: f64,
    /// 单独覆盖的模型。**覆盖价不受倍率影响**
    #[serde(default)]
    pub models: std::collections::BTreeMap<String, PriceFields>,
}

fn one() -> f64 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PriceSheetSave {
    pub sheet: PriceSheetInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
    /// 保存之后使用这张价目表的上游。给了就**恰好是这几家**：列表里的改用
    /// 它，原来用它、不在列表里的改回默认价目表，和价目表本身在同一个版本
    /// 里写入。不给就不动上游的选择
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_by: Option<Vec<String>>,
}

/// 按哪张价目表查价。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SheetRef {
    /// 默认价目表
    Default,
    /// 一张已经保存的价目表
    Named { name: String },
    /// 编辑中、还没保存的那一张
    Draft { sheet: PriceSheetInput },
}

/// 查价。**界面不自己实现计价顺序**，要显示什么价就来问。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PriceQuery {
    pub sheet: SheetRef,
    /// 要查的模型。给了就只查这些
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// 没给模型时按名字搜：默认价目表里的模型，加上这张价目表单独覆盖的
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    /// 搜索最多返回几个。默认 50
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// 一个模型查到的价格。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ResolvedPrice {
    pub model: String,
    /// 价格和来源。`None` = 无法计价
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<PriceFields>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PriceSourceView>,
    /// 价格是从别的平台借来的。**按它算出来的钱是估算**
    pub estimated: bool,
    /// 上下文窗口
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PriceQueryResult {
    pub items: Vec<ResolvedPrice>,
    /// 按名字搜时，一共有多少个模型对得上（`items` 可能被 `limit` 截断）
    pub matched: usize,
}

/// 光标落在配置的哪一段上。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ConfigAt {
    /// `providers` / `groups` / `routes` / `clients`…
    pub section: Option<String>,
    /// 那一项的名字。**不给下标** —— 下标对界面没有意义，而且用户重排
    /// 之后它指向另一个东西
    pub name: Option<String>,
}

/// 整份文本写回去（文本模式）。
///
/// **和 `PATCH` 是两条路，但同一扇门。**表单模式改字段，文本模式改整份
/// —— 后者是前者的退路（结构性的增删一律引导到文本模式），而两者
/// 都必须带 `base_version`，都会走那三道校验。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ConfigWrite {
    pub base_version: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ConfigWritten {
    pub version: String,
}

// ─────────────────────────────────────────────── 上游与代理的增删改

/// 新建或修改一个上游时交过来的定义。
///
/// **结构，不是 YAML。**以前界面拼一段 YAML 交给补丁接口 —— 拼字符串的
/// 那一方不知道引号规则，一个带 `#` 的值就能写坏整份配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProviderInput {
    pub name: String,
    /// 修改时**不给就是保持原样**：视图里的地址是打过码的，原样写回去
    /// 会把码写进配置
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// API 密钥。修改时默认保持原样 —— 界面拿不到原值，也不该拿到
    #[serde(default)]
    pub key: SecretChange,
    /// 请求头，按顺序。**整张表就是保存之后的样子**：没列出来的行被删掉，
    /// 某一行不给 `value` 表示沿用同名那一行的原值
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<HeaderInput>,
    /// OAuth。修改时默认保持原样
    #[serde(default)]
    pub oauth: OAuthChange,
    /// `anthropic` / `openai-chat` / `openai-responses` / `gemini`。
    /// 不给就按地址推断
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
    /// `direct` / `system` / 代理名
    #[serde(default = "direct")]
    pub proxy: String,
    /// `fail` / `direct`
    #[serde(default = "fail_closed")]
    pub on_proxy_fail: OnProxyFail,
    /// 服务不提供模型列表时的手动清单
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// 启用范围：只用这些模型（ID 或 glob）。不给就是它提供的全部
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_only: Option<Vec<String>>,
    /// `per-token` / `free`。不给就是按量计费
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing: Option<Billing>,
    /// 按哪张价目表计价。不给就是默认价目表
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<String>,
    /// 停用
    #[serde(default)]
    pub disabled: bool,
}

/// 按接口地址自动识别的结果。给编辑中、还没保存的上游显示「自动识别」
/// 会选成什么。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProviderPreviewRequest {
    pub base_url: String,
    /// 表单里选定的协议。不给就是「自动识别」。只影响 `auth_header`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProviderPreview {
    /// 按地址推断的接口协议。推断不出是空（转发时按 Anthropic 处理）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
    /// API 密钥放在哪个请求头里：`x-api-key` / `authorization` / `x-goog-api-key`。
    /// 选定了协议按选定的算，否则按推断出的
    pub auth_header: String,
}

slug_enum! {
    /// 最近一次向上游获取模型清单的结果。
    pub enum ModelListStatus {
        /// 还没获取。停用的上游一直是这样
        Pending = "pending",
        /// 上游列出了清单
        Listed = "listed",
        /// 问到了，但上游不提供清单（没有这个接口、格式认不出、空的）
        NoList = "no_list",
        /// 没问到：连不上、凭据被拒、取不到凭据
        Failed = "failed",
    }
}

/// 一个上游的模型清单。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProviderModelsView {
    pub provider: String,
    /// `discovered`（上游列出的）/ `manual`（手动清单）/ `none`
    pub source: ModelSource,
    /// 最近一次获取的结果，同 [`ProviderView::model_status`]
    pub status: ModelListStatus,
    /// 正在获取
    pub fetching: bool,
    /// 最近一次向上游获取清单的时间。还没获取过是空
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at_ms: Option<u64>,
    /// 没从上游拿到清单的原因
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Msg>,
    pub models: Vec<ModelRow>,
}

/// 页面打开时补问模型清单：开始问的是哪几家。答案随 `models_changed` 到。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ModelsRefreshing {
    pub providers: Vec<String>,
}

/// 清单里的一个模型。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ModelRow {
    pub id: String,
    /// 在启用范围里
    pub enabled: bool,
    /// 上下文窗口，来自默认价目表
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// 按这个上游选的价目表查到的价格。空 = 无法计价
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<PriceFields>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_source: Option<PriceSourceView>,
    /// 价格是从别的平台借来的。**按它算出来的钱是估算**
    pub estimated: bool,
}

fn direct() -> String {
    "direct".to_string()
}

fn fail_closed() -> OnProxyFail {
    OnProxyFail::Fail
}

/// 一个密钥类的值怎么改。**三态**，因为视图里拿不到原值：不动就得有「保持原样」。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SecretChange {
    #[default]
    Keep,
    None,
    /// 可以写 `${ENV}` 从环境变量读
    Set {
        value: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct HeaderInput {
    pub name: String,
    /// 不给表示沿用同名那一行的原值
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum OAuthChange {
    #[default]
    Keep,
    None,
    Set {
        refresh: String,
        endpoint: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_secret: Option<String>,
        /// 现成的 access token。**检测一份还没保存的 OAuth 凭据只能用它**
        /// —— 拿 refresh token 去换，服务端可能当场作废旧的那把，而新的
        /// 那把还没有地方写
        #[serde(default, skip_serializing_if = "Option::is_none")]
        access: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProviderSave {
    pub provider: ProviderInput,
    /// 你基于哪一版。**对不上就是 409**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
}

/// 检测一个上游，**不保存**。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProviderTest {
    pub provider: ProviderInput,
    /// 正在编辑的是哪一家。给了的话，表单里没改的凭据和地址从它那儿取
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProxyInput {
    pub name: String,
    /// `socks5h` / `socks5` / `http` / `https`
    pub kind: ProxyKind,
    /// `host:port`
    pub addr: String,
    pub auth: ProxyAuthInput,
}

/// 代理的认证怎么处理。
///
/// **三态**，因为视图里拿不到原来的用户名和密码：编辑时不动认证，就得有
/// 一个「保持原样」的说法，而不是把空值当成「清掉」。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ProxyAuthInput {
    /// 保持原来的认证（新建时等同于不需要认证）
    Keep,
    /// 不需要认证
    None,
    /// 设成这一对
    Set { user: String, pass: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProxySave {
    pub proxy: ProxyInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
}

/// 检测一个代理，**不保存**。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProxyTest {
    pub proxy: ProxyInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
}

// ─────────────────────────────────────────────── 路由与策略组的增删改

/// 新建或修改一条路由时交过来的定义。**规则的顺序就是数组的顺序。**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RouteInput {
    pub name: String,
    #[serde(default)]
    pub rules: Vec<RuleInput>,
}

/// 一条规则的定义。
///
/// **条件和视图是同一套写法**（[`ConditionView`]）：界面拿到什么，就交回
/// 什么，不必再学一种格式。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RuleInput {
    pub name: String,
    /// 空 = 兜底，匹配全部请求
    #[serde(default)]
    pub conditions: Vec<ConditionView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// 拒绝，以及返回给客户端的原因
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<RuleRewrite>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RouteSave {
    pub route: RouteInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
    /// 保存之后使用这条路由的密钥。给了就**恰好是这几把**：列表里的改用它，
    /// 原来用它、不在列表里的改用默认路由，和路由本身在同一个版本里写入。
    /// 不给就不动密钥的选择
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys: Option<Vec<String>>,
    /// 和路由一起写入：把这几类客户端辅助请求设为「交给路由」。
    ///
    /// **规则里的辅助请求条件只对交给路由的类别生效** —— 其余类别的请求在
    /// 进路由之前就被本地应答或原样放行，不带类别标记，那个条件永远不满足
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub route_probes: Vec<String>,
}

/// 删除一条路由。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RouteDelete {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
    /// 使用这条路由的密钥改用哪一条。不给 = 改用默认路由（不指定路由）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reassign_to: Option<String>,
}

/// 更换默认路由。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct DefaultRouteSave {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
}

slug_enum! {
    /// 策略组按什么排候选：配置里 `type` 写的那个词。
    pub enum GroupKind {
        /// 按顺序，前一个不可用才用下一个
        Fallback = "fallback",
        /// 用选中的那一个，不可用时按顺序
        Select = "select",
        /// 轮流
        LoadBalance = "load-balance",
        /// 选最快的
        UrlTest = "url-test",
        /// 选最便宜的
        Cheapest = "cheapest",
    }
}

/// 新建或修改一个策略组时交过来的定义。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct GroupInput {
    pub name: String,
    pub kind: GroupKind,
    /// 成员，按顺序
    pub providers: Vec<String>,
    /// `select` 组优先使用的成员
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
    /// 同一次会话固定走同一家。只对 `load-balance` 有意义
    #[serde(default = "default_true")]
    pub session_affinity: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct GroupSave {
    pub group: GroupInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
}

/// 网关知道的一个模型，以及能提供它的上游（已按启用范围与停用过滤）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct KnownModel {
    pub id: String,
    pub providers: Vec<String>,
}

/// 聚合类端点的时间窗（`GET /summary`、`/latency`）。**缺省是「今天」而不是
/// 「最近 24 小时」** —— 用户问的是「今天花了多少」，那是个从零点算起的问题。
///
/// **查询串里的类型都把字段摊平写，不用 `#[serde(flatten)]`。**flatten 会让
/// serde 走 `deserialize_any`，而查询串里一切都是字符串 —— `from_ms=123` 被当成
/// 字符串喂给 `i64`，整个请求 400。只在同时带上时间窗时才失败，单元测试看不见。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Window {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_ms: Option<i64>,
}

/// `GET /summary/buckets`：时间窗 + 桶宽。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct BucketQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket_ms: Option<i64>,
}

/// `GET /summary/buckets/by`：再按一个维度分组。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct BucketGroupQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket_ms: Option<i64>,
    /// 和 `GroupQuery` 同一条理由：是枚举不是字符串，它会变成列名。
    pub dim: CostDim,
}

/// `GET /summary/by`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct GroupQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_ms: Option<i64>,
    /// **是枚举不是字符串。**它会决定 SQL 里的列名，用字符串就是一个
    /// 注入口；写错的值在这里被 serde 直接拒掉，而不是拼进查询。
    pub dim: CostDim,
}

/// 一张列表要的两样：看哪一段，最多几条（`GET /history`、`/sessions`）。
///
/// **时间窗是可选的，而且缺省不是「今天」。**「最近 N 条」本身就是一个完整
/// 的回答，而缺省成今天的话，过了零点这张表会空掉。两端各自可缺：只给
/// `from_ms` 就是「从那时起到现在」。条数缺省 200、最多 2000。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ListQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// 安全日志一页要的：哪一项、哪一段、从哪条往前、几条（`GET /security/events`）。
/// 缺省是全部，不是今天；条数缺省 100、最多 500。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SecurityEventsQuery {
    /// 只要这一项的。不给是全部
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<Guard>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_ms: Option<i64>,
    /// 只要这条之前的（翻页）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// `GET /config/at`：配置原文里的第几个字节落在哪个字段上。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ConfigAtQuery {
    pub offset: usize,
}

/// 删除时带上的版本。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct BaseVersion {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
}

/// 历史里的一版。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ConfigVersion {
    pub version: String,
    pub at_ms: u64,
    /// `ui` / `cli` / `external` / `rollback` / `rotation`
    pub origin: ConfigOrigin,
    pub bytes: u64,
    /// 这一版是现在跑着的那一版吗。
    ///
    /// **历史里包括当前版本**，所以列表最上面那条通常就是它 —— 不标
    /// 出来的话，用户会以为第一条是「上一版」然后回滚到自己身上。
    #[serde(default)]
    pub current: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RollbackRequest {
    pub version: String,
}

/// 一段时间的汇总。
///
/// **实测和估算分开，没有价格的单独数。**「今日 $12.40 实测 + ~$0.80
/// 估算，另有 3 条没有价格」比一个混在一起的 $13.20 诚实得多 —— 后者
/// 看起来是个确定的数字。
/// 一个时间桶的花费与请求数（概览的趋势图）。
///
/// **成本三态在这里不合并**：实测、估算、以及没有价格的条数。
/// 把第三种当成 0 加进柱子，那根柱子就是偏低的，而看图的人没有线索
/// 知道少算了什么。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CostBucket {
    /// 桶的起点
    pub at_ms: i64,
    pub requests: i64,
    pub failed: i64,
    pub cost_micros_exact: i64,
    pub cost_micros_estimated: i64,
    /// 价目表里没有这个模型的条数（用量是有的），见 `Summary::unpriced_requests`
    pub unpriced_requests: i64,
    /// 没有拿到用量的条数，见 `Summary::no_usage_requests`
    pub no_usage_requests: i64,
}

/// 一个时间桶里，某一个模型（或上游）的那部分。
///
/// **和 `CostBucket` 是两个查询，不是一个的扩展。**趋势图要回答的是
/// 「什么时候花的」和「花在哪个模型上」—— 合成一张按模型分层的图之后，
/// 这两个问题只用看一次；而把它们拆成一张趋势图加一张构成图，读的人
/// 要在两张图之间自己对时间。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CostBucketGroup {
    pub at_ms: i64,
    /// 模型名、上游名或密钥名，看查的是哪一维
    pub name: String,
    pub requests: i64,
    pub failed: i64,
    pub cost_micros_exact: i64,
    pub cost_micros_estimated: i64,
    /// 这一格里这一项有几条请求的模型不在价目表里（用量是有的），见
    /// `Summary::unpriced_requests`。
    ///
    /// **每一项自己带着。**这一项的金额是 0 时，要分得清「没有价格」和「确实
    /// 没花钱」—— 只有整格的数的话，说不出缺着钱的是哪一项。
    pub unpriced_requests: i64,
    /// 这一格里这一项有几条请求没有拿到用量，见 `Summary::no_usage_requests`
    pub no_usage_requests: i64,
    /// 这一格里这一项用掉的 token。
    ///
    /// **四类分开给。**它们的单价差十倍以上，加成一个数之后既算不回
    /// 钱，也说不清「这段时间是在写新上下文还是在吃缓存」。
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
}

/// 按模型或上游分组的花费（钱花在哪儿）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CostGroup {
    pub name: String,
    pub requests: i64,
    pub cost_micros: i64,
    /// 价目表里没有这个模型的条数（用量是有的）
    pub unpriced_requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// 没有拿到用量的条数
    pub no_usage_requests: i64,
}

/// 一段时间里各条路由走了多少请求、各条规则命中了多少，以及这些数从哪一刻起是全的
/// （`GET /summary/routes`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RouteStats {
    /// 这段时间里记录从哪一刻起是全的，Unix 毫秒：问的起点和库里最老那条请求开始的
    /// 时刻，取晚的那个。
    ///
    /// **数只数得到库里还在的请求。**刚装好、升级时库重建了、记录留的天数比窗口短，
    /// 库里最老的那条都比窗口的起点晚：更早的那一截不是「没有命中」，是「不知道」。
    /// 「这段时间一次都没命中」只对这一刻之后成立。
    ///
    /// - 等于问的起点：整段都有记录
    /// - 比起点晚：记录从这一刻才开始，[`RouteStats::routes`] 数的是从这一刻到窗口结束
    /// - 空：这段时间里没有一刻有记录 —— 库里一条请求都没有，或者最老的那条也在窗口
    ///   结束之后。这时的「没有命中」什么都说明不了
    ///
    /// 有值时总在窗口里面：不早于问的起点，早于窗口的终点。
    pub covered_since_ms: Option<i64>,
    /// 各条路由，走得多的在前
    pub routes: Vec<RouteHits>,
}

/// 一条路由在一段时间里走了多少请求，各条规则命中了多少。
///
/// **按请求落库时记下的路由算**，不按现在的配置推：一个请求走的是它那一刻的
/// 路由和规则，之后改名、删掉、换了绑定都不改它。所以这里可能有配置里已经没有的
/// 路由和规则，而配置里有、这段时间一次都没命中的不在这里 —— 「从来没命中过」
/// 就是在配置里有、在这里找不到（只对 [`RouteStats::covered_since_ms`] 之后成立）。
///
/// 数的是经过路由的请求：本地应答的不算（它们没到规则那一层）；规则还没做出决定
/// 就失败了的也不算 —— 鉴权没过、模型不让用、没有一条规则命中。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RouteHits {
    pub route: String,
    /// 走这条路由的请求数
    pub requests: i64,
    /// 其中失败的。**被规则拒绝的也算**；客户端取消的不算（同 `Summary::failed`）
    pub failed: i64,
    /// 最后一个请求是什么时候开始的，Unix 毫秒
    pub last_ms: i64,
    /// 这条路由里各条规则命中了多少，多的在前。
    ///
    /// **一个请求可以算在几条规则上**：决定去向的那一条（转发或拒绝）、附加了参数
    /// 改写的每一条、选定上游之后拒绝了它的那一条。所以各条加起来可以比
    /// `requests` 多；决定去向的规则每个请求恰好一条
    pub rules: Vec<RuleHits>,
}

/// 一条规则命中了多少请求。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RuleHits {
    pub rule: String,
    /// 它决定了去向（转发或拒绝）的请求数
    pub decided: i64,
    /// 它命中的请求数：决定了去向的、附加了改写的、选定上游之后拒绝了的，一个
    /// 请求只算一次
    pub requests: i64,
    /// 其中失败的（同 `RouteHits::failed`）
    pub failed: i64,
    /// 最后一次命中的请求是什么时候开始的，Unix 毫秒
    pub last_ms: i64,
}

/// 分组维度。**是个枚举不是字符串** —— 它最终来自 query string，
/// 而把它拼进 SQL 的列名里就是一个注入口。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "lowercase")]
pub enum CostDim {
    Model,
    Provider,
    /// 按网关密钥。**密钥是不可伪造的那个身份** —— `client_hint` 来自请求头，
    /// 谁都能写；而这一列是网关自己按密钥反查出来的
    Client,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Summary {
    pub requests: i64,
    pub failed: i64,
    /// 本地应答的次数。**是个正向数字**，单独显示
    pub locally_answered: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// 单位是微分（百万分之一美元）
    pub cost_micros_exact: i64,
    pub cost_micros_estimated: i64,
    /// 有多少条请求**价目表里没有它的模型**：用量是有的，缺的是单价。
    /// **不是 0，是「不知道」。**给那个模型配一个价格就能解决。
    ///
    /// 失败的、没有用量的、不计费的都不在这里 —— 配价格对它们没用。
    pub unpriced_requests: i64,
    /// 有多少条请求**没有拿到用量**，所以同样算不出钱：上游没报，或者连接
    /// 在它报之前就结束了（客户端取消、WebSocket 会话）。
    ///
    /// 和 `unpriced_requests` 一样让金额合计偏低，但配价格解决不了它 ——
    /// 界面上是两句不同的话。上游确实接下了的才算：成功的响应和客户端
    /// 取消的，失败的和上游回了 4xx 的不算。
    pub no_usage_requests: i64,
    /// 用了缓存之后净省下多少微分。
    ///
    /// **净额：命中节省的部分，减去写入产生的溢价。**用户想知道的是
    /// 「如果完全不用缓存，这段时间要多花还是少花」—— 而缓存写入按
    /// 1.25 倍单价计费，所以这个数可以是负的。
    pub cache_saved_micros: i64,
    /// 本区间两项防护各留下了几条记录。**和安全日志数的是同一批** —— 概览上
    /// 点开这个数，落到的日志就是这么多条
    pub security: SecurityCounts,
    /// 价目表的快照日期。**成本旁边要标它** —— 一个两个月前
    /// 的价目表算出来的数字，可信度和昨天的完全不同
    pub pricing_date: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct LatencyView {
    pub model: String,
    pub p50: i64,
    pub p95: i64,
    /// **样本数要一起给。**「800ms」是 3 个样本还是 300 个，含义完全不同
    pub samples: usize,
}

/// 一条历史请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct HistoryRow {
    pub id: i64,
    pub at_ms: i64,
    pub client: String,
    pub provider: String,
    pub model: String,
    pub path: String,
    pub status: Option<u16>,
    pub ttfb_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    pub bytes: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub cost_micros: Option<i64>,
    /// 这个成本是估的吗。**界面上要标出来**
    pub cost_estimated: bool,
    /// 失败的原因。**带着码** —— 翻历史时界面照样能说自己那句话；
    pub error: Option<Msg>,
    /// 本地应答的
    pub local: bool,
    /// 客户端没等到响应结束就走了。**不是失败**（`error` 是空的）；用量
    /// 只算到断开那一刻，所以有金额的话一定是估算
    pub cancelled: bool,
    /// 服务它的那家怎么收钱：`per-token` / `free`。本地应答的是 `free`：网关
    /// 自己答的，费用确实是零
    pub billing: Billing,
    /// 缓存命中省下了多少微分。`None` = 算不出来
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_saved_micros: Option<i64>,
    /// 路由决策与尝试链。本地应答的没有它：它没到规则那一层。路由还没报出
    /// 结论请求就结束了的（上游应答之前客户端就走了）有开始时就知道的那几项，
    /// 尝试链是空的
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<RoutingView>,
    /// 按什么价格算的。没算出金额的没有它
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_source: Option<PriceSourceView>,
    /// 服务它的那一跳做过的格式转换。直通的没有它
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translated: Option<TranslatedView>,
    /// 它属于哪一次会话，和 [`SessionView::id`] 是同一个值。
    ///
    /// **请求和会话是同一批记录的两个粒度**，而不带这个字段的话，界面
    /// 上的两个粒度之间就没有门：看着一条很贵的请求，问不出它属于哪次
    /// 任务；看着一次很贵的任务，也回不到具体是哪一条。库里这一列一直
    /// 都在（`requests.session`，还建了索引），只是没有交出来。
    ///
    /// 认不出会话的请求（拼不出指纹的，比如 WebSocket、本地应答）是 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// 按请求头推测是哪个应用发的（`claude-code`、`codex`…）。**可以伪造**，
    /// 只用来显示；身份是 `client` 那把密钥
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_hint: Option<String>,
    /// 非本机来的请求的来源地址。本机来的没有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    /// 请求带的那把网关密钥打码后的样子（`tw-re…wb4e`），请求那一刻的
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_masked: Option<String>,
    /// 请求带着 DeepSeek Harness 的会话日志：它的字节数。没带的没有，理由见
    /// `Event::RequestStarted::session_log_bytes`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_log_bytes: Option<i64>,
    /// 这次请求在两项防护上留下的记录（和安全日志同一份）。没有就是空的。
    ///
    /// **流量页的徽标靠它。**以前徽标只来自实时事件，关窗再开就没了 ——
    /// 而那正是用户回头翻「那一条到底被换了什么」的时候。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub security: Vec<SecurityEventView>,
}

/// 一次请求做过的格式转换。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TranslatedView {
    /// 客户端的格式：`anthropic` / `openai-chat` / `openai-responses` / `gemini`
    pub from: Dialect,
    /// 服务它的上游的格式
    pub to: Dialect,
    /// 客户端请求里转不过去、被丢掉的字段路径
    #[serde(default)]
    pub dropped: Vec<String>,
}

/// 一条请求的全部细节。**详情抽屉吃这个。**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RequestDetail {
    pub row: HistoryRow,
    pub request_body: Option<BodyView>,
    pub response_body: Option<BodyView>,
    /// 这个请求还在跑。**记录在结局到了才落库**，这时的 `row` 是到目前为止
    /// 知道的那些：开始时的身份和上游，响应头到了就有状态码，路由走完就有
    /// 尝试链；耗时、用量、金额都还没有。请求体已经存下了，响应体要等结局。
    /// 结局到了再取一次，就是完整的那一份
    pub in_flight: bool,
}

/// 一份存下来的 body。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct BodyView {
    /// **已脱敏**。这段文字会被复制到 issue 里
    pub text: String,
    /// 原本多长。**截断了要能说出来** —— 不说的话用户会以为请求本身
    /// 就长这样
    pub original_len: usize,
    pub truncated: bool,
}

/// 一个上游的订阅额度。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProviderQuota {
    pub provider: String,
    pub windows: Vec<QuotaWindow>,
}

// ---------------------------------------------------------------- ChatGPT 账号

/// 发起 ChatGPT 登录（`POST /chatgpt/login`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ChatgptLoginStart {
    /// 登录后写进配置的上游名。不给就是 `chatgpt`；已经有同名的 ChatGPT 账号上游时，
    /// 换掉它的凭据（重新登录），其余设置不动
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 换 token 走哪条路：`direct` / `system` / 代理名。不给是 `direct`。新建上游时也写成它的出站方式
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// 登录完成后，浏览器页面跳到哪里。**只接受应用自己的协议**（`thinkwatch://…`），
    /// 不接受网页地址。设备码登录没有浏览器页面，给了也不用
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_to: Option<String>,
    /// 在哪台设备上授权：`browser`（默认，在这台机器上开浏览器）或 `device`（拿一个
    /// 一次性码，去别的设备上输）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ChatgptLoginMode>,
}

/// 一次进行中的登录。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ChatgptLogin {
    pub id: String,
    /// 在浏览器里打开的授权地址。`browser` 登录才有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorize_url: Option<String>,
    /// 要用户输进去的一次性码。`device` 登录才有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
    /// 用户在另一台设备上打开、输码的地址。`device` 登录才有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_url: Option<String>,
    /// 多少秒内要完成
    pub expires_in_secs: u64,
}

/// 登录进行到哪一步（`GET /chatgpt/login/{id}`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ChatgptLoginStatus {
    pub id: String,
    /// `pending` / `done` / `failed` / `expired` / `cancelled`
    pub status: LoginStatus,
    /// 写进配置的上游名。`done` 时有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// 登的是哪个账号：邮箱和套餐。`done` 时有，令牌里一项都读不出来时没有。
    ///
    /// **和上游视图的 [`OAuthView::account`] 是同一块、同一个来源**：从这次登录存进
    /// 配置的 access token 里读。登录完成时说的账号和上游那一行说的是同一个，不用
    /// 再去读一次概览
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountView>,
    /// `failed` 时的原因
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Msg>,
}

/// ChatGPT 账号的用量（`GET /providers/{name}/chatgpt/usage`）。
///
/// **登的是谁不在这里**，在上游视图的 [`OAuthView::account`]：那一份从本机的凭据里读，
/// 不用为了给一行上游标上账号去问一次后端
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ChatgptUsage {
    /// 额度窗口，词表同 [`QuotaWindow`]
    pub windows: Vec<QuotaWindow>,
    /// 可用的额度重置卡张数。账号没有这一项时没有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_credits: Option<i64>,
}

/// 一张额度重置卡。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ResetCreditView {
    pub id: String,
    /// 重置哪种额度，后端的原词
    pub reset_type: String,
    /// 后端的原词
    pub status: String,
    pub granted_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// 账号上的额度重置卡（`GET /providers/{name}/chatgpt/resets`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ResetCredits {
    pub available_count: i64,
    pub credits: Vec<ResetCreditView>,
}

/// 用一张额度重置卡（`POST /providers/{name}/chatgpt/resets`）。
///
/// **卡用掉就回不来**，所以只在用户明确点下去时发，网关自己从不用。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ResetCreditUse {
    /// 幂等键。同一次操作重试时用同一个值，后端不会重复扣卡
    pub idempotency_key: String,
    /// 用哪一张。不给由后端挑
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ResetCreditUsed {
    /// - `reset`：额度已重置
    /// - `nothing_to_reset`：额度没用完，不需要重置，没有扣卡
    /// - `no_credit`：没有可用的卡（指定了卡时：那张已经不能用）
    /// - `already_redeemed`：这个幂等键已经用过，额度在那一次已经重置
    pub code: String,
    /// 重置了几个窗口
    #[serde(default)]
    pub windows_reset: i64,
}

/// 用 Z.ai 或 BigModel 的账号登录（`POST /zai/login`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ZaiLoginStart {
    /// 登哪一家：`zai`（api.z.ai）或 `bigmodel`（open.bigmodel.cn）。不给是 `zai`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<ZaiFamily>,
    /// 登录后写进配置的上游名。不给就是那一家的名字；已经有同名的同一家账号上游时，
    /// 换掉它的密钥（重新登录），其余设置不动
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 登录期间调它们的接口走哪条路：`direct` / `system` / 代理名。不给是 `direct`。
    /// 新建上游时也写成它的出站方式
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
}

/// 一次进行中的 Z.ai 登录。
///
/// **没有回到应用的地址**：授权完成后浏览器停在对方自己的页面上，那一页不是我们的，
/// 我们只能靠轮询知道登录成了。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ZaiLogin {
    pub id: String,
    /// 在浏览器里打开的授权地址
    pub authorize_url: String,
    /// 多少秒内要完成
    pub expires_in_secs: u64,
}

/// 登录进行到哪一步（`GET /zai/login/{id}`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ZaiLoginStatus {
    pub id: String,
    /// `pending` / `done` / `failed` / `expired` / `cancelled`
    pub status: LoginStatus,
    /// 写进配置的上游名。`done` 时有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// 登的是哪个账号，邮箱或者昵称。`done` 时有，对方没给就没有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// `failed` 时的原因
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Msg>,
}

/// L3 测速要花多少。
///
/// **这是「你确认要花钱吗」那个对话框的全部内容。**触发前必须显示它，
/// 而不是点了才知道。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SpeedEstimate {
    pub provider: String,
    pub model: String,
    /// 输入 token。**精确值** —— 请求是固定的
    pub input_tokens: u64,
    /// 输出上限。**空 = 这家不接受输出上限**（ChatGPT 账号的 Codex 后端），
    /// 那时按量计费的 `cost_micros` 也是空的 —— 回答有多长由模型决定
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// 微分。按量计费算得出来时是那个数，不计费时是 0，无法计价时是空
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<i64>,
    /// 这家的计费方式：`per-token` / `free`
    pub billing: Billing,
    /// 这家服务不了这个模型：`out_of_scope`（不在启用范围里）/
    /// `not_offered`（模型清单里没有）。有值时不进合计，也不会被测
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<ServeSkip>,
}

/// 一批测速的账。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SpeedQuote {
    pub items: Vec<SpeedEstimate>,
    /// 总计。有一项算不出来就是 None —— 给一个看起来完整的数字，用户会
    /// 以为那就是全部代价
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_micros: Option<i64>,
    pub pricing_date: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SpeedRunRequest {
    /// 测哪几家。空 = 所有上游
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<String>,
    /// **必填。**同一个 provider 的 Opus 和 Haiku 是两条完全不同的曲线，
    /// 不指定模型的测速结果没有意义
    pub model: String,
}

/// 一次 L3 测速的结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SpeedResult {
    pub provider: String,
    pub model: String,
    pub ok: bool,
    pub connect_ms: u64,
    /// **首 token。**这一层唯一值得测的东西
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    pub total_ms: u64,
    /// 实际消耗。**和预估对照** —— 有些上游会附加 system prompt
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Msg>,
}

/// 观测这一层在不在记。
///
/// **不看磁盘还剩多少。**那是操作系统的事，网关管好自己占的那一份就够了
/// —— 正文按天数和总量回收（见 `tw_store::blobs`），摘要按天数回收。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct StorageStatus {
    /// 请求记录启动了没有。`false` = 数据库打不开之类，这段时间的请求都不会留下
    pub recording: bool,
    /// 记了多少条
    pub rows: i64,
    /// 请求体占了多少字节
    pub blob_bytes: u64,
    /// **转发受影响了吗。永远是 false** —— 观测挂了，代理照跑
    pub forwarding_affected: bool,
}

// ---------------------------------------------------------------- 会话

/// 一次任务。
///
/// **孤立地看单个请求，看不出任何有用的东西** —— Claude Code 的一次任务
/// 是几十到上百个请求，携带不断增长的上下文。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SessionView {
    pub id: String,
    pub client: String,
    pub started_ms: u64,
    pub ended_ms: u64,
    pub turns: u64,
    /// 有价格的那些轮次加起来，单位是**微分**
    pub cost_micros: i64,
    /// 其中估算的那部分。**不为 0 时，合计要标成估算** —— 估算不能冒充实测
    pub cost_micros_estimated: i64,
    /// 算出了价格的轮数。**一轮都没有时，合计不是 $0，是「没有价格」**
    pub priced_turns: u64,
    /// **价目表里没有那个模型的轮数。**「$1.23」和「$1.23，另有 4 轮没有
    /// 价格」是两个不同的结论
    pub unpriced_turns: u64,
    /// 没有拿到用量、所以算不出钱的轮数
    pub no_usage_turns: u64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// 缓存命中省下了多少，微分
    pub cache_saved_micros: i64,
    /// 上下文峰值。**一眼看出哪次任务的上下文失控了**
    pub peak_input_tokens: i64,
    pub models: Vec<String>,
    pub errors: u64,
}

/// 会话里的一轮。上下文增长曲线和成本瀑布画的就是它。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TurnView {
    pub id: i64,
    pub at_ms: u64,
    pub model: String,
    pub provider: String,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    /// **没有价格就是 None，不是 0**
    pub cost_micros: Option<i64>,
    pub duration_ms: Option<i64>,
    pub error: Option<Msg>,
    /// 客户端没等到这一轮结束就走了（见 `HistoryRow::cancelled`）
    pub cancelled: bool,
    /// 这一轮的金额是估算。**瀑布图上要带记号** —— 以前这里没有这个字段，
    /// 估算的金额在瀑布图上和实测的长得一模一样
    pub cost_estimated: bool,
    /// 服务它的那家怎么收钱，和 `HistoryRow::billing` 同一套词：`per-token` /
    /// `free`
    pub billing: Billing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SessionDetail {
    pub session: SessionView,
    pub turns: Vec<TurnView>,
}

// ---------------------------------------------------------------- 请求重放

/// 把存下来的那条请求，原样发给另一个上游。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ReplayRequest {
    pub id: i64,
    pub provider: String,
}

/// 报价。**按下确认之前必须看到它**（和 L3 测速同一条纪律）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ReplayQuote {
    pub model: String,
    pub provider: String,
    pub body_bytes: i64,
    pub input_tokens: i64,
    /// `None` = 这个模型无法计价。**不是 0**
    pub cost_micros: Option<i64>,
    /// 要重放到的那家的计费方式：`per-token` / `free`
    pub billing: Billing,
    /// 发出去之前会不会脱敏。用户有权在按下去之前知道
    pub will_redact: bool,
    pub pricing_date: String,
}

/// 原来那一次长什么样，用来并排比。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ReplayOriginal {
    pub provider: String,
    pub status: Option<u16>,
    pub ttfb_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    pub bytes: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ReplayResult {
    pub provider: String,
    pub status: u16,
    pub ttfb_ms: i64,
    pub duration_ms: i64,
    pub bytes: i64,
    /// 响应正文，**已还原占位符、已脱敏、已截断**
    pub body: String,
    pub original: ReplayOriginal,
}

// ---------------------------------------------------------------- 路由试算

/// 「如果现在来这样一个请求，会走到哪儿」。
///
/// **每个字段都对应规则里能写的一个条件**。默认值就是一个最
/// 普通的请求 —— 用户只需要改他关心的那一两个。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct DryRunRequest {
    pub model: String,
    /// 哪把密钥发的。只给它时按这把密钥使用的路由求值；规则里的 `client`
    /// 条件也按它判断
    #[serde(default)]
    pub client: String,
    /// 按这条路由求值，不管密钥使用哪一条
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    /// 按一份还没保存的路由求值。给了就不看 `route`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<RouteInput>,
    #[serde(default = "default_dialect")]
    pub dialect: Dialect,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    /// 带了 `cache_control`。**把它路由到不支持缓存的中转站，等于把最大
    /// 的省钱手段直接扔掉，而且不会察觉**
    #[serde(default)]
    pub cache: bool,
    #[serde(default)]
    pub tools: bool,
    #[serde(default)]
    pub tool_count: usize,
    #[serde(default)]
    pub image: bool,
    #[serde(default)]
    pub thinking: bool,
    #[serde(default = "default_true")]
    pub stream: bool,
    /// 客户端自己发的辅助请求。空 = 真实的用户请求
    #[serde(default)]
    pub intent: String,
}

fn default_dialect() -> Dialect {
    Dialect::Anthropic
}
fn default_true() -> bool {
    true
}

slug_enum! {
    /// 试算时一条规则的结论。
    pub enum RuleVerdict {
        Matched = "matched",
        Skipped = "skipped",
        /// 条件要等选定上游之后才能求值，静态试算给不了结论
        PhaseTwo = "phase_two",
    }
}

slug_enum! {
    /// 一条命中的规则起了什么作用。
    pub enum RuleEffect {
        /// 决定了去向
        Decide = "decide",
        /// 附加了改写
        Apply = "apply",
        /// 去向已由前面的规则决定，也没有附加项
        None = "none",
    }
}

/// 一条规则在这次试算里的下场。**没命中的也要列出来，并说清为什么** ——
/// 「为什么没走我以为的那条」和「走了哪条」是同一个问题的两面。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RuleTrace {
    pub name: String,
    /// `matched` | `skipped` | `phase_two`（条件要等选定上游之后才能求值，
    /// 静态试算给不了结论）
    pub verdict: RuleVerdict,
    /// 没命中时，第一个没对上的条件
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mismatch: Option<MismatchView>,
    /// 条件本身写错了、没法求值时的说明
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Msg>,
    /// 命中时它起了什么作用：`decide`（决定了去向）/ `apply`（附加了改写或
    /// 安全要求）/ `none`（去向已由前面的规则决定，也没有附加项）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<RuleEffect>,
}

/// 一个没对上的条件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct MismatchView {
    /// 和 `ConditionView.field` 同一个词表
    pub field: ConditionField,
    /// 规则里写的值。`intent` 写了多个时逐个列出
    pub want: Vec<String>,
    /// 这个请求实际的值。`intent` 为空表示真实的用户请求
    pub got: String,
}

/// 一项参数改写。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SetView {
    /// `model`（换模型，整个 prompt cache 作废）/ `max_tokens` / `thinking` /
    /// `only_at_session_start`（以上改写只在新会话开始时应用，值是 `true`）
    pub field: SetField,
    pub value: String,
}

slug_enum! {
    /// 一次试算的结论。
    pub enum DryRunOutcome {
        /// 走到了某个上游或组
        Route = "route",
        /// 被规则拒绝
        Deny = "deny",
        /// 没有规则命中
        NoMatch = "no_match",
        /// 选中的上游都服务不了，见 `skipped`
        Unavailable = "unavailable",
        /// 客户端的辅助请求，网关自己答
        Intercepted = "intercepted",
        /// 客户端的辅助请求，原样转发
        Passthrough = "passthrough",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct DryRunResult {
    /// 按哪条路由求的值。草稿是草稿的名字
    pub route: String,
    /// 经过的组按什么排候选：`fallback` / `select` / `load-balance` /
    /// `url-test` / `cheapest`。
    ///
    /// **不说的话，用户看不懂候选为什么是这个顺序** —— 「我明明把官方
    /// 写在第一个」。直指 provider 时是 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<GroupKind>,
    /// `route` | `deny` | `no_match` | `unavailable`（选中的上游都服务不了，
    /// 见 `skipped`）| `intercepted` | `passthrough`
    ///
    /// **后两个说的是这个请求压根没到规则那一层。**客户端自己发的辅助
    /// 请求先过 `client_probes`：本地应答的一个字节都不出本机，原样放行的
    /// 直接转发 —— 两种情况下 `trace` 都是空的，因为确实一条规则都没求值。
    pub outcome: DryRunOutcome,
    /// 命中的规则名
    pub rule: Option<String>,
    /// `deny` 时规则里写的拒绝理由
    pub reason: Option<String>,
    /// 候选链，第一个是首选，后面是故障转移的备选
    pub candidates: Vec<String>,
    /// 经过了哪个组
    pub via_group: Option<String>,
    /// 累积起来的参数改写
    pub set: Vec<SetView>,
    pub trace: Vec<RuleTrace>,
    /// 这条路会不会伤到 prompt cache。**要直说 —— 它决定账单**
    pub hurts_cache: bool,
    /// 候选链里此刻熔断着的那些。**试算是静态的，但熔断是当下的事实**
    pub circuit_open: Vec<String>,
    /// 规则选中、但服务不了这个请求而被跳过的上游
    pub skipped: Vec<SkippedView>,
    /// 候选链里要转换格式的上游：客户端的格式和上游的协议不同
    pub converted: Vec<ConvertedView>,
}

/// 一个要转换格式的候选上游。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ConvertedView {
    pub provider: String,
    /// 客户端的格式：`anthropic` / `openai-chat` / `openai-responses` / `gemini`
    pub from: Dialect,
    /// 这个上游的格式
    pub to: Dialect,
}

/// 一个被跳过的候选上游。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SkippedView {
    pub provider: String,
    /// `disabled` / `out_of_scope` / `not_offered`
    pub reason: ServeSkip,
}

// ---------------------------------------------------------- 客户端
//
// 接管、还原、MCP、扫描在桌面端做（改的是它那台机器上的文件），类型也在那边。
// 这里只剩「为某个客户端发一把专用密钥」。

/// 为某个客户端准备的那把网关密钥（`POST /clients/{id}/key`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ClientKey {
    pub name: String,
    /// 明文。这一步就是为了拿去填进客户端
    pub key: String,
    /// 这次新建的（此前没有为它留着的）
    pub created: bool,
}

// ---------------------------------------------------------------- 安全

/// 出站脱敏找到的一项：哪条规则、哪个值（已打码）、在这个请求里出现了几次。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SecretItem {
    /// 内置规则的 id（`anthropic-api-key` …），或者自定义规则的名字
    pub rule: String,
    #[serde(default)]
    pub custom: bool,
    /// 类别：`api-keys` / `private-keys` / `jwt` / `conn-strings` / `internal` / `custom`
    pub kind: SecretKind,
    /// **已打码。**报出来的东西一律打码 —— 「发现了 sk-ant-xxx」这句话本身
    /// 就是一次泄漏。内网地址和内部域名例外，它们不是凭据
    pub masked: String,
    pub count: u64,
}

/// 藏匿字符的一种：哪一种、在哪儿、几处、第一个长什么样。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct HiddenItem {
    /// `tag`（Unicode 标签字符）/ `bidi`（双向控制符）
    pub kind: HiddenKind,
    /// 在工具结果里，而不是调用方自己打的字
    pub in_tool_result: bool,
    pub count: u64,
    /// 第一个的码位，写成 `U+E0049`
    pub example: String,
    /// 标签字符解出来的原文（最多 120 个字符）：**藏的是什么**。双向控制符是空的
    pub revealed: String,
}

slug_enum! {
    /// 哪一项防护。配置里 `security` 下的那个键，也是接口路径里的那一段。
    pub enum Guard {
        /// 出站脱敏
        Redact = "redact",
        /// 工具调用审查
        InspectTools = "inspect_tools",
        /// 藏匿字符
        HiddenText = "hidden_text",
        /// 内容过滤
        Content = "content",
        /// 输出长度
        OutputLimit = "output_limit",
    }
}

slug_enum! {
    /// 一条规则在拦截档下做什么。工具调用审查是 `cut` / `record`，内容过滤是
    /// `block` / `record`；别的防护命中之后做什么由档位决定，没有这一项。
    pub enum RuleAction {
        /// 切断这个工具调用所在的流（工具调用审查）
        Cut = "cut",
        /// 拒绝这个请求，不发出去（内容过滤）
        Block = "block",
        /// 只记录
        Record = "record",
    }
}

/// 各项防护在一段时间里各留下了几条记录。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SecurityCounts {
    /// 出站脱敏找到的（每条 = 一个请求里的一个值）
    pub secrets: i64,
    /// 其中已替换的（拦截档）
    pub secrets_replaced: i64,
    /// 命中规则的工具调用
    pub tool_calls: i64,
    /// 其中被切断的
    pub tool_calls_cut: i64,
    /// 藏匿字符（每条 = 一个请求里一种藏法在一个地方）
    pub hidden_text: i64,
    /// 其中请求被拒的
    pub hidden_text_blocked: i64,
    /// 命中内容规则的（每条 = 一个请求命中一条规则）
    pub content: i64,
    /// 其中请求被拒的
    pub content_blocked: i64,
    /// 回答超过输出长度的
    pub output_limit: i64,
    /// 其中被切断的
    pub output_limit_cut: i64,
}

/// 安全日志的一条。
///
/// **一条是一次命中**：出站脱敏是「一个请求里的一个值」（出现几次合成
/// 一条，`count` 说几次），工具调用审查是「一个工具调用命中一条规则」。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SecurityEventView {
    pub id: i64,
    pub at_ms: i64,
    pub request_id: i64,
    pub guard: Guard,
    /// 内置规则的 id，或者自定义规则的名字。藏匿字符是那一种（`tag` / `bidi`），
    /// 输出长度是 `max_chars`
    pub rule: String,
    #[serde(default)]
    pub custom: bool,
    /// 做了什么：`recorded`（只记录）/ `replaced`（已替换）/ `cut`（已切断）/
    /// `blocked`（请求被拒，没有发出去）
    pub action: SecurityOutcome,
    /// 请求最终由哪个上游服务；还没结束的是当时的首选
    pub provider: String,
    /// 哪把网关密钥
    pub client: String,
    /// 请求的模型。还没落库的请求是空的
    #[serde(default)]
    pub model: String,
    /// 工具调用审查：哪个工具。藏匿字符和内容过滤：在工具结果里时是 `tool_result`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// 出站脱敏是打码后的值；工具调用审查、内容过滤是命中的那一小段（已截断）；
    /// 藏匿字符是第一个的码位，标签字符后面跟一个空格和解出来的原文；输出长度是上限
    pub excerpt: String,
    /// 出站脱敏：这个值在请求里出现了几次。藏匿字符：几个字符。输出长度：超的那一刻
    /// 数到了多少个字符。其余是 1
    pub count: i64,
    /// 按请求头推测是哪个应用发的（`claude-code`、`codex`…）。**可以伪造**，
    /// 只用来显示；身份是 `client` 那把密钥
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_hint: Option<String>,
    /// 非本机来的请求的来源地址。本机来的没有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    /// 请求带的那把网关密钥打码后的样子（`tw-re…wb4e`），请求那一刻的
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_masked: Option<String>,
}

/// 安全日志的一页。**按时间倒序**，`more` 说后面还有没有。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SecurityEventsPage {
    pub events: Vec<SecurityEventView>,
    pub more: bool,
    /// 这一段时间里、按这一项筛出来的一共几条 —— **整段的，不只是这一页**。
    ///
    /// `before` 是翻页的位置，不是筛选：翻到第几页，这个数都一样。页头的
    /// 「N 次命中」是它；拿读到的条数去数，读满一页就只能写「100+」
    pub total: i64,
    /// `total` 里各做了什么。四项加起来就是 `total`
    pub by_outcome: SecurityOutcomeCounts,
}

/// 一段安全日志里，每一种做法各几条（见 [`SecurityOutcome`]）。没有的是 0，
/// 四项都在。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SecurityOutcomeCounts {
    /// 只记录
    pub recorded: i64,
    /// 已替换成占位符
    pub replaced: i64,
    /// 已切断
    pub cut: i64,
    /// 请求被拒，没有发出去
    pub blocked: i64,
}

/// 一条内置规则按什么认。**给界面说明用**，界面按类型写成自己的话。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Matcher {
    /// 以 `prefix` 开头，其后至少还有 `min_tail` 个字符
    Prefix { prefix: String, min_tail: usize },
    /// `sk-` 开头的 OpenAI 老式密钥：全长至少 `min_len`，字母和数字都有
    OpenaiLegacy { min_len: usize },
    /// PEM 私钥块，BEGIN 到对应的 END 整段
    Pem,
    /// 三段 base64url，首段解码后含 `"alg"`
    Jwt,
    /// `协议://用户:口令@主机` 里的口令
    ConnString,
    /// RFC1918 私有地址，不含回环
    PrivateIp,
    /// 以这几个后缀结尾的域名
    DomainSuffix { suffixes: Vec<String> },
    /// 正则表达式：工具调用审查的全部规则，和各项防护的自定义规则
    Regex { pattern: String },
    /// 不分大小写的子串：内容过滤的关键词规则
    Contains { text: String },
    /// 这几段码位里的字符：藏匿字符的两种，写成 `U+E0000–U+E007F`
    Codepoints { ranges: Vec<String> },
}

/// 一条规则。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SecurityRuleView {
    /// 内置规则的 id，或者自定义规则的名字
    pub id: String,
    #[serde(default)]
    pub custom: bool,
    /// 英文名。界面按 id 查自己的名称表，查不到才用它；自定义规则就是名字
    pub name: String,
    /// 为什么值得看一眼（英文）。出站脱敏和自定义规则没有
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
    /// 类别。出站脱敏：`api-keys` … `custom`；工具调用审查：`command` / `custom`；
    /// 内容过滤：`injection` / `persona` / `chinese` / `custom`；藏匿字符：`invisible`
    pub kind: String,
    pub matcher: Matcher,
    pub enabled: bool,
    /// 出厂时开不开。自定义规则是 `true`
    pub on_by_default: bool,
    /// 工具调用审查、内容过滤：拦截档下做什么
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<RuleAction>,
    /// 内置规则出厂时拦截档下做什么。和 `action` 不一样就是改过
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_action: Option<RuleAction>,
}

/// 一项防护的档位和规则。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct GuardDetail {
    /// `off` / `observe` / `enforce`
    pub mode: GuardMode,
    /// 按界面上的顺序：内置的在前，自定义的在后
    pub rules: Vec<SecurityRuleView>,
}

/// 输出长度的档位和上限。它没有规则，只有一个数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct OutputLimitDetail {
    /// `off` / `observe` / `enforce`
    pub mode: GuardMode,
    /// 上限，按字符数
    pub max_chars: u64,
    /// 出厂的上限
    pub default_max_chars: u64,
    /// 最多能设多大
    pub ceiling: u64,
}

/// 各项防护。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SecurityDetail {
    pub redact: GuardDetail,
    pub inspect_tools: GuardDetail,
    /// 规则就是那两种藏法，可以各自关掉
    pub hidden_text: GuardDetail,
    pub content: GuardDetail,
    pub output_limit: OutputLimitDetail,
}

/// 改档位。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ModeSave {
    /// `off` / `observe` / `enforce`
    pub mode: GuardMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
}

/// 启用或停用一条规则（内置的或自定义的）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RuleToggle {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
}

/// 改一条内置规则在拦截档下做什么。只有工具调用审查和内容过滤的规则有这一项 ——
/// 别的防护命中之后做什么由档位决定。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ActionSave {
    /// 工具调用审查：`cut` / `record`；内容过滤：`block` / `record`
    pub action: RuleAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
}

/// 新建或修改一条自定义规则。改的时候名字可以变，那就是改名。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CustomRuleSave {
    pub name: String,
    pub pattern: String,
    /// 工具调用审查：`cut` / `record`；内容过滤：`block` / `record`。不给按 `record`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<RuleAction>,
    /// 内容过滤才有：`contains`（不分大小写的子串）/ `regex`。不给按 `contains`。
    /// 别的防护的自定义规则都是正则
    #[serde(rename = "match", default, skip_serializing_if = "Option::is_none")]
    pub matching: Option<ContentMatch>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
}

fn yes() -> bool {
    true
}

/// 改输出长度的上限。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct LimitSave {
    /// 按字符数，1 到 [`OutputLimitDetail::ceiling`]
    pub max_chars: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
}

/// 拿一段文本试一试。给了 `pattern` 就只试这一条正则，给了 `rule` 就只试
/// 这一条内置规则（停用着的也能试），都不给就按现在启用的全部规则。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SecurityTestRequest {
    pub sample: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// 内容过滤试 `pattern` 时怎么认：`contains` / `regex`，不给按 `contains`
    #[serde(rename = "match", default, skip_serializing_if = "Option::is_none")]
    pub matching: Option<ContentMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
}

/// 试出来的一处。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SecurityTestHit {
    pub rule: String,
    #[serde(default)]
    pub custom: bool,
    /// 在样本里的位置，**按 UTF-16 码元计** —— 界面是 JavaScript，按它的
    /// 下标切就能标出来
    pub start: usize,
    pub end: usize,
    /// 出站脱敏：打码后的值；工具调用审查、内容过滤：命中的那一小段；藏匿字符：
    /// 那个字符的码位
    pub excerpt: String,
    /// 工具调用审查、内容过滤：拦截档下做什么
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<RuleAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SecurityTestResult {
    pub hits: Vec<SecurityTestHit>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 枚举化的字段在线上仍然是那个词，`slug()` 说的也是它。
    #[test]
    fn closed_sets_keep_their_words_on_the_wire() {
        fn check<
            T: Copy + Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
        >(
            all: &[T],
            slug: fn(T) -> &'static str,
            from: fn(&str) -> Option<T>,
        ) {
            for &v in all {
                let wire = serde_json::to_value(v).unwrap();
                assert_eq!(wire, serde_json::Value::from(slug(v)), "{v:?}");
                assert_eq!(serde_json::from_value::<T>(wire).unwrap(), v);
                assert_eq!(from(slug(v)), Some(v));
            }
            assert!(from("no-such-word").is_none());
        }
        check(Billing::ALL, Billing::slug, Billing::from_slug);
        check(Protocol::ALL, Protocol::slug, Protocol::from_slug);
        check(Dialect::ALL, Dialect::slug, Dialect::from_slug);
        check(ProxyKind::ALL, ProxyKind::slug, ProxyKind::from_slug);
        check(OnProxyFail::ALL, OnProxyFail::slug, OnProxyFail::from_slug);
        check(ModelSource::ALL, ModelSource::slug, ModelSource::from_slug);
        check(Health::ALL, Health::slug, Health::from_slug);
        check(
            BreakerState::ALL,
            BreakerState::slug,
            BreakerState::from_slug,
        );
        check(ProxyState::ALL, ProxyState::slug, ProxyState::from_slug);
        check(AuthState::ALL, AuthState::slug, AuthState::from_slug);
        check(
            FailureSource::ALL,
            FailureSource::slug,
            FailureSource::from_slug,
        );
        check(GuardMode::ALL, GuardMode::slug, GuardMode::from_slug);
        check(
            SecurityOutcome::ALL,
            SecurityOutcome::slug,
            SecurityOutcome::from_slug,
        );
        check(
            ContentMatch::ALL,
            ContentMatch::slug,
            ContentMatch::from_slug,
        );
        check(SecretKind::ALL, SecretKind::slug, SecretKind::from_slug);
        check(HiddenKind::ALL, HiddenKind::slug, HiddenKind::from_slug);
        check(LoginStatus::ALL, LoginStatus::slug, LoginStatus::from_slug);
        check(
            ChatgptLoginMode::ALL,
            ChatgptLoginMode::slug,
            ChatgptLoginMode::from_slug,
        );
        check(
            KnownChatgptPlan::ALL,
            KnownChatgptPlan::slug,
            KnownChatgptPlan::from_slug,
        );
        check(ZaiFamily::ALL, ZaiFamily::slug, ZaiFamily::from_slug);
        check(
            ConfigOrigin::ALL,
            ConfigOrigin::slug,
            ConfigOrigin::from_slug,
        );
        check(ConfigStage::ALL, ConfigStage::slug, ConfigStage::from_slug);
        check(
            PricingSource::ALL,
            PricingSource::slug,
            PricingSource::from_slug,
        );
        check(ProbeClass::ALL, ProbeClass::slug, ProbeClass::from_slug);
        check(ProbeMode::ALL, ProbeMode::slug, ProbeMode::from_slug);
        check(
            ConditionField::ALL,
            ConditionField::slug,
            ConditionField::from_slug,
        );
        check(SetField::ALL, SetField::slug, SetField::from_slug);
        check(ServeSkip::ALL, ServeSkip::slug, ServeSkip::from_slug);
        check(
            AttemptOutcome::ALL,
            AttemptOutcome::slug,
            AttemptOutcome::from_slug,
        );
        check(L1Step::ALL, L1Step::slug, L1Step::from_slug);
        check(L1Peer::ALL, L1Peer::slug, L1Peer::from_slug);
        check(
            L1SkipReason::ALL,
            L1SkipReason::slug,
            L1SkipReason::from_slug,
        );
        check(
            ModelListStatus::ALL,
            ModelListStatus::slug,
            ModelListStatus::from_slug,
        );
        check(GroupKind::ALL, GroupKind::slug, GroupKind::from_slug);
        check(RuleVerdict::ALL, RuleVerdict::slug, RuleVerdict::from_slug);
        check(RuleEffect::ALL, RuleEffect::slug, RuleEffect::from_slug);
        check(
            DryRunOutcome::ALL,
            DryRunOutcome::slug,
            DryRunOutcome::from_slug,
        );
        check(Guard::ALL, Guard::slug, Guard::from_slug);
        check(RuleAction::ALL, RuleAction::slug, RuleAction::from_slug);
        assert_eq!(GroupKind::LoadBalance.slug(), "load-balance");
        assert_eq!(Guard::InspectTools.slug(), "inspect_tools");
    }

    #[test]
    fn events_carry_a_discriminating_tag() {
        // 前端按 `kind` 分派。少了它，TypeScript 那边只能靠字段有无来猜。
        let e = Event::RequestFinished {
            id: 1,
            model: "claude-sonnet-5".into(),
            status: 200,
            bytes: 10,
            duration_ms: 5,
            usage: None,
        };
        let v: serde_json::Value = serde_json::to_value(&e).unwrap();
        assert_eq!(v["kind"], "request_finished");
        assert_eq!(v["id"], 1);
        // 结局自己带着模型名：开始之后才来听的一方只有它
        assert_eq!(v["model"], "claude-sonnet-5");
    }

    #[test]
    fn every_event_exposes_its_request_id() {
        // UI 靠 id 把四个事件缝成一行。
        for e in [
            Event::RequestStarted {
                key_masked: None,
                peer: None,
                id: 7,
                client: "c".into(),
                client_hint: None,
                session: None,
                route: "default".into(),
                rule: "catch-all".into(),
                group: None,
                rewritten_by: vec![],
                provider: "p".into(),
                billing: Billing::PerToken,
                model: "m".into(),
                method: "POST".into(),
                path: "/v1/messages".into(),
                session_log_bytes: None,
                at_ms: 0,
            },
            Event::RequestHeaders {
                id: 7,
                status: 200,
                ttfb_ms: 1,
            },
            Event::RequestFinished {
                id: 7,
                model: String::new(),
                status: 200,
                bytes: 1,
                duration_ms: 1,
                usage: None,
            },
            Event::RequestFailed {
                id: 7,
                model: String::new(),
                source: FailureSource::Upstream,
                message: tw_types::msg!("t.x" => "x"),
                bytes: None,
                duration_ms: None,
                usage: None,
            },
            Event::RequestCancelled {
                id: 7,
                model: String::new(),
                status: Some(200),
                bytes: 1,
                duration_ms: 1,
                usage: None,
            },
        ] {
            assert_eq!(e.id(), 7);
        }
    }

    /// 开始事件带着会话和路由的第一阶段：界面不用等落库、不用按现在的配置去猜。
    /// 没有的就不出现（认不出的会话、直接指上游的规则没有策略组）
    #[test]
    fn a_start_says_its_session_and_where_it_is_headed() {
        let e = Event::RequestStarted {
            key_masked: None,
            peer: None,
            id: 7,
            client: "c".into(),
            client_hint: None,
            session: Some("abc-def-1000".into()),
            route: "工作".into(),
            rule: "opus 走官方".into(),
            group: None,
            rewritten_by: vec!["关掉思考".into()],
            provider: "官方".into(),
            billing: Billing::PerToken,
            model: "m".into(),
            method: "POST".into(),
            path: "/v1/messages".into(),
            session_log_bytes: None,
            at_ms: 0,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["session"], "abc-def-1000");
        assert_eq!(v["route"], "工作");
        assert_eq!(v["rule"], "opus 走官方");
        assert_eq!(v["rewritten_by"], serde_json::json!(["关掉思考"]));
        assert!(v.get("group").is_none(), "{v}");
        assert!(v.get("session_fp").is_none(), "{v}");
    }

    /// `/in-flight` 是一个对象：core 的时钟，加上每个在跑的请求到目前为止的事件，
    /// 事件照事件流上的样子
    #[test]
    fn the_in_flight_snapshot_carries_the_clock_and_each_requests_events() {
        let snap = InFlight {
            now_ms: 5_000,
            requests: vec![InFlightRequest {
                id: 3,
                events: vec![Event::RequestHeaders {
                    id: 3,
                    status: 200,
                    ttfb_ms: 40,
                }],
            }],
        };
        let v = serde_json::to_value(&snap).unwrap();
        assert_eq!(v["now_ms"], 5_000);
        assert_eq!(v["requests"][0]["id"], 3);
        assert_eq!(v["requests"][0]["events"][0]["kind"], "request_headers");
        let back: InFlight = serde_json::from_value(v).unwrap();
        assert_eq!(back.requests[0].events[0].id(), 3);
    }

    /// 取消带着用量走 —— **存储层要拿它算钱**。而客户端在第一帧之前就走了
    /// 的那种，字段整个不出现，不是一组零。
    #[test]
    fn a_cancellation_carries_the_usage_seen_so_far() {
        let e = Event::RequestCancelled {
            id: 3,
            model: String::new(),
            status: Some(200),
            bytes: 512,
            duration_ms: 2400,
            usage: Some(UsageView {
                input: 5000,
                output: 1,
                ..Default::default()
            }),
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["kind"], "request_cancelled");
        assert_eq!(v["usage"]["input"], 5000);
        let back: Event = serde_json::from_value(v).unwrap();
        assert!(matches!(
            back,
            Event::RequestCancelled { usage: Some(u), .. } if u.input == 5000
        ));

        // 响应头之前就走了的：状态码和用量都不出现，不是 0
        let none = Event::RequestCancelled {
            id: 4,
            model: String::new(),
            status: None,
            bytes: 0,
            duration_ms: 10,
            usage: None,
        };
        let v = serde_json::to_value(&none).unwrap();
        assert!(v.get("usage").is_none(), "{v}");
        assert!(v.get("status").is_none(), "{v}");
    }

    #[test]
    fn status_round_trips() {
        let s = Status {
            api_version: CONTROL_API_VERSION,
            version: "2026.9.0".into(),
            pid: 1,
            gateway_addr: Some("127.0.0.1:8788".into()),
            listen_error: None,
            config_rejected: None,
            config_path: "/x/config.yaml".into(),
            clients: 1,
            providers: 1,
            uptime_secs: 0,
            in_flight: 3,
            remote_control: RemoteControlView::default(),
            gateway_reachable: vec![],
        };
        let back: Status = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.gateway_addr.as_deref(), Some("127.0.0.1:8788"));
        assert_eq!(back.in_flight, 3);
    }

    #[test]
    fn safe_mode_is_representable() {
        // 安全模式 = 控制面在、数据面不在。它必须是一个能被表达的状态，
        // 而不是「gateway_addr 是空字符串」这种约定。
        let json = r#"{"api_version":1,"version":"x","pid":1,"gateway_addr":null,
                       "config_path":"/x","clients":0,"providers":0,"uptime_secs":0,
                       "in_flight":0,"gateway_reachable":[],
                       "remote_control":{"enabled":false,"addr":null,"allow_from":[],"reachable":[]}}"#;
        let s: Status = serde_json::from_str(json).unwrap();
        assert!(s.gateway_addr.is_none());
    }

    /// 套餐的集合是 OpenAI 的：认得的归到一个词上，认不出来的原样过去，线上都是那个词。
    #[test]
    fn a_plan_is_the_backends_word_known_or_not() {
        use serde_json::json;
        let plus = ChatgptPlan::from_raw("Plus").unwrap();
        assert_eq!(plus, ChatgptPlan::Known(KnownChatgptPlan::Plus));
        assert_eq!(serde_json::to_value(&plus).unwrap(), json!("plus"));
        // 同一个套餐的另一种写法
        assert_eq!(
            ChatgptPlan::from_raw("hc"),
            Some(ChatgptPlan::Known(KnownChatgptPlan::Enterprise))
        );
        assert_eq!(
            ChatgptPlan::from_raw(" education "),
            Some(ChatgptPlan::Known(KnownChatgptPlan::Edu))
        );
        for &k in KnownChatgptPlan::ALL {
            assert_eq!(ChatgptPlan::from_raw(k.slug()), Some(ChatgptPlan::Known(k)));
        }
        // 新名字不是「未知」，是它自己
        let new = ChatgptPlan::from_raw("pro_ultra").unwrap();
        assert_eq!(new, ChatgptPlan::Other("pro_ultra".into()));
        assert_eq!(serde_json::to_value(&new).unwrap(), json!("pro_ultra"));
        // 读回来：认得的是枚举，认不出来的仍是原词
        assert_eq!(
            serde_json::from_value::<ChatgptPlan>(json!("team")).unwrap(),
            ChatgptPlan::Known(KnownChatgptPlan::Team)
        );
        assert_eq!(
            serde_json::from_value::<ChatgptPlan>(json!("pro_ultra")).unwrap(),
            new
        );
        for blank in ["", "  "] {
            assert_eq!(ChatgptPlan::from_raw(blank), None, "{blank:?}");
        }
    }

    /// 令牌里没写的项不出现；没有账号的凭据连这一块都没有。
    #[test]
    fn an_account_says_only_what_the_token_said() {
        use serde_json::json;
        let a = AccountView {
            email: Some("someone@example.com".into()),
            plan: None,
        };
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            json!({"email": "someone@example.com"})
        );
        let mut o = OAuthView {
            endpoint: "https://auth.example/token".into(),
            client_id: None,
            expires_at: None,
            failure: None,
            needs_login: false,
            account: None,
        };
        assert!(serde_json::to_value(&o).unwrap().get("account").is_none());
        o.account = Some(AccountView {
            email: None,
            plan: ChatgptPlan::from_raw("pro"),
        });
        let back: OAuthView = serde_json::from_value(serde_json::to_value(&o).unwrap()).unwrap();
        assert_eq!(back, o);
    }
}

/// 数据目录在哪。
///
/// **在契约层，不在 tw-config。**core 和桌面端必须落到同一个目录 —— 端口文件、
/// socket、配置（连同控制面的钥匙）都在里面，两边各算一遍、算得不一样的话，界面找不到一个正在
/// 跑的网关。以前桌面端自己只看 `HOME`，Windows 上那个变量默认不存在，于是它
/// 落到当前目录下的 `.thinkwatch`，而 core 在 `%APPDATA%\ThinkWatch`。控制面
/// 的地址（`control::Address`）因为同样的理由搬到了这里。
pub mod data {
    use std::path::PathBuf;

    /// 按哪套习惯去放数据目录。
    ///
    /// **是个参数，不是就地一个 `cfg!`。**这样两边的判断在同一台机器上都测得到
    /// —— 写错的那一支往往是本机没在跑的那一支。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Convention {
        /// `$HOME/.thinkwatch`
        Unix,
        /// `%APPDATA%\ThinkWatch`
        Windows,
    }

    impl Convention {
        /// 这台机器用哪一套。
        pub const HERE: Self = if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        };
    }

    /// 数据目录：配置、请求库、锁、控制面的 socket 或端口文件都在这里。
    ///
    /// `THINKWATCH_HOME` 最优先 —— 测试靠它隔离，用户靠它换地方。
    ///
    /// 没有它就按平台的习惯：unix 上是 `~/.thinkwatch`；Windows 上是
    /// `%APPDATA%\ThinkWatch`，**不是 `%USERPROFILE%\.thinkwatch`** —— 点开头的
    /// 目录是 unix 的习惯，而在 Windows 上，用户要去找一个程序存了什么的时候
    /// 是去 APPDATA 找的。
    ///
    /// **和「用户的 home」不是一回事**，后者用来顺着去找各家客户端的配置
    /// （`~/.claude`、`~/.codex` 这些点开头的目录在 Windows 上确实躺在
    /// `%USERPROFILE%` 下），见 tw-control 的 `home_dir`。
    pub fn dir() -> PathBuf {
        pick(
            Convention::HERE,
            std::env::var_os("THINKWATCH_HOME"),
            std::env::var_os("HOME"),
            std::env::var_os("APPDATA"),
        )
    }

    fn pick(
        conv: Convention,
        explicit: Option<std::ffi::OsString>,
        home: Option<std::ffi::OsString>,
        appdata: Option<std::ffi::OsString>,
    ) -> PathBuf {
        // 设成空串当作没设。`PathBuf::from("")` 是一个谁都解释不了的路径，而它
        // 会一路走到「打不开这个文件」才暴露出来。
        if let Some(e) = explicit.filter(|v| !v.is_empty()) {
            return PathBuf::from(e);
        }
        let base = match conv {
            Convention::Unix => home.filter(|v| !v.is_empty()).map(|h| (h, ".thinkwatch")),
            Convention::Windows => appdata.filter(|v| !v.is_empty()).map(|a| (a, "ThinkWatch")),
        };
        match base {
            Some((dir, leaf)) => PathBuf::from(dir).join(leaf),
            // 环境里什么都问不出来：退到当前目录下的一个相对路径。**不退到根目录**
            // —— 那会让我们往系统盘里写东西。
            None => PathBuf::from(".thinkwatch"),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::ffi::OsString;

        fn os(s: &str) -> Option<OsString> {
            Some(OsString::from(s))
        }

        /// 显式指定的那个谁也推不翻它 —— 测试靠它隔离，推翻了测试就会去写
        /// 真正的 `~/.thinkwatch`。
        #[test]
        fn thinkwatch_home_wins_on_either_platform() {
            for conv in [Convention::Unix, Convention::Windows] {
                assert_eq!(
                    pick(
                        conv,
                        os("/somewhere/else"),
                        os("/home/x"),
                        os("C:\\AppData")
                    ),
                    PathBuf::from("/somewhere/else"),
                    "{conv:?}"
                );
            }
        }

        #[test]
        fn unix_puts_it_under_home_and_windows_under_appdata() {
            assert_eq!(
                pick(Convention::Unix, None, os("/home/x"), os("C:\\AppData")),
                PathBuf::from("/home/x/.thinkwatch")
            );
            assert_eq!(
                pick(Convention::Windows, None, os("/home/x"), os("C:\\AppData")),
                PathBuf::from("C:\\AppData").join("ThinkWatch")
            );
        }

        /// 两边各看各的变量。**Windows 上的 `HOME` 通常根本不存在**，而在装了
        /// Git Bash 的机器上它又存在 —— 两种情况都不该影响数据目录去哪儿。
        #[test]
        fn neither_platform_reads_the_other_ones_variable() {
            assert_eq!(
                pick(Convention::Unix, None, os("/home/x"), None),
                PathBuf::from("/home/x/.thinkwatch"),
                "unix 不需要 APPDATA"
            );
            assert_eq!(
                pick(Convention::Windows, None, os("/home/x"), None),
                PathBuf::from(".thinkwatch"),
                "Windows 上有 HOME 也不能拿它当 APPDATA 使"
            );
        }

        /// 设成空串等于没设。`PathBuf::from("")` 要到「打不开这个文件」才暴露。
        #[test]
        fn an_empty_variable_counts_as_unset() {
            assert_eq!(
                pick(Convention::Unix, os(""), os("/home/x"), None),
                PathBuf::from("/home/x/.thinkwatch")
            );
            assert_eq!(
                pick(Convention::Unix, None, os(""), None),
                PathBuf::from(".thinkwatch")
            );
        }

        /// 什么都问不出来时退到相对路径，**不是根目录**。
        #[test]
        fn with_nothing_to_go_on_it_stays_relative() {
            for conv in [Convention::Unix, Convention::Windows] {
                let got = pick(conv, None, None, None);
                assert_eq!(got, PathBuf::from(".thinkwatch"), "{conv:?}");
                assert!(!got.is_absolute(), "{conv:?} 落到了绝对路径：{got:?}");
            }
        }

        /// 这台机器上那一套是哪一套。
        #[test]
        fn here_follows_the_platform_it_was_compiled_for() {
            #[cfg(windows)]
            assert_eq!(Convention::HERE, Convention::Windows);
            #[cfg(not(windows))]
            assert_eq!(Convention::HERE, Convention::Unix);
        }
    }
}

/// 控制面在哪儿、拿哪把钥匙进门。
///
/// **这是契约的一部分，不是两边各自的约定。**core 在这儿听，桌面端到这儿连
/// —— 以前两边各拼一次 `<数据目录>/twcore.sock`，能对上只是因为那一行足够
/// 短。Windows 上这个答案要分岔（那里没有 unix socket），两份各写一次就是
/// 两份会漂，而漂掉的表现是「界面连不上一个正在跑的网关」。
///
/// **仍然没有 IO**，遵守这个 crate 的规矩（见模块头）：这里只有名字和形状，
/// 真正去绑、去读的是各自那一侧。
pub mod control {
    use std::path::{Path, PathBuf};

    /// unix socket 文件，在数据目录下。
    pub const SOCKET_FILE: &str = "twcore.sock";
    /// Windows 上控制面绑到哪个端口，由 core 写、由客户端读。
    pub const PORT_FILE: &str = "control.port";
    /// 数据目录下的配置文件。控制面的钥匙就写在它里面（[`KEY_PATH`]）。
    pub const CONFIG_FILE: &str = "config.yaml";
    /// 钥匙在配置里的位置：`listen.control.key`。
    ///
    /// **两边拼的是同一个位置**：core 生成、补上、打码都照它找，桌面端读
    /// 也照它找。各写一遍字符串，漂掉的表现是「界面拿着空钥匙去敲门」。
    pub const KEY_PATH: [&str; 3] = ["listen", "control", "key"];
    /// 钥匙写成多少个十六进制字符。32 字节。
    pub const KEY_HEX_LEN: usize = 64;
    /// 配置正文发给界面、写进历史时，钥匙换成这一串。
    ///
    /// **和一把真钥匙一样长**（64 个字符）：界面按光标位置问「这是哪一段」
    /// （`GET /config/at`），打码前后长度一样，后面每一处的偏移才对得上。
    /// 它不是十六进制，所以不会被错当成一把钥匙；整份写回时带着它，就是
    /// 「钥匙不动」。
    pub const KEY_MASK: &str = "hidden-see-twcore-control-key-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";

    /// 控制面听在哪，客户端到哪儿连。
    ///
    /// 叫 `Address` 不叫 `Endpoint`：[`crate::Endpoint`] 是控制面上的一个端点。
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Address {
        /// 一个 `0700` 的 unix socket 文件。**权限是文件系统给的**，所以这
        /// 一档天然只有当前用户连得上。
        Socket(PathBuf),
        /// 回环上的一个端口。Windows 只有这一档。
        ///
        /// **端口不固定**：固定端口会和别的软件撞，也省了想连进来的人一步。
        /// core 绑到一个系统给的空闲端口，再把号码写进这个文件。
        ///
        /// 回环端口挡不住同机的任何进程，也问不出对端是谁（unix socket 问
        /// 得出，`SO_PEERCRED`）—— 那一档的门**全靠握手**。
        Loopback { port_file: PathBuf },
    }

    impl Address {
        /// 这个平台默认听在数据目录的什么位置。
        pub fn in_dir(dir: &Path) -> Self {
            #[cfg(unix)]
            {
                Address::Socket(dir.join(SOCKET_FILE))
            }
            #[cfg(not(unix))]
            {
                Address::Loopback {
                    port_file: dir.join(PORT_FILE),
                }
            }
        }
    }

    /// 配置文件在哪。
    pub fn config_file(dir: &Path) -> PathBuf {
        dir.join(CONFIG_FILE)
    }

    /// 控制面的钥匙：32 个随机字节，配置里写成 64 个十六进制字符。
    ///
    /// 它是握手（`tw-link`，Noise 的 PSK）的全部凭据：不知道它的一方连第一条
    /// 握手消息都解不开。**住在契约层而不是 tw-link**：tw-config 要按它校验
    /// 配置，而 tw-config 不该为了一个格式检查背上整套加密库。生成随机数的
    /// 那一步在 tw-config（它本来就依赖 rand），这里只有格式，仍然没有 IO。
    #[derive(Clone, PartialEq, Eq)]
    pub struct ControlKey([u8; 32]);

    /// 一串文字不是一把钥匙的原因。
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum KeyFormatError {
        /// 长度不对。`len` 是去掉首尾空白之后的字符数
        Length { len: usize },
        /// 有不是十六进制的字符
        NotHex,
    }

    impl std::fmt::Display for KeyFormatError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                KeyFormatError::Length { len } => write!(
                    f,
                    "the control key is {len} characters long; it has to be {KEY_HEX_LEN} \
                     hexadecimal characters"
                ),
                KeyFormatError::NotHex => write!(
                    f,
                    "the control key has characters that are not hexadecimal; it has to be \
                     {KEY_HEX_LEN} of 0-9 and a-f"
                ),
            }
        }
    }

    impl std::error::Error for KeyFormatError {}

    impl ControlKey {
        /// 按配置里的写法读：正好 64 个十六进制字符，大小写都认，首尾空白不算。
        ///
        /// **短了不是「弱一点」，是不收。**一把好猜的钥匙和没有钥匙差不多，
        /// 而这把钥匙守着能改全部配置、能关掉 core 的那扇门。
        pub fn parse(s: &str) -> Result<Self, KeyFormatError> {
            let s = s.trim();
            let len = s.chars().count();
            if len != KEY_HEX_LEN || s.len() != KEY_HEX_LEN {
                return Err(KeyFormatError::Length { len });
            }
            let mut out = [0u8; 32];
            for (i, pair) in s.as_bytes().chunks(2).enumerate() {
                let hi = hex_digit(pair[0]).ok_or(KeyFormatError::NotHex)?;
                let lo = hex_digit(pair[1]).ok_or(KeyFormatError::NotHex)?;
                out[i] = (hi << 4) | lo;
            }
            Ok(Self(out))
        }

        pub fn from_bytes(bytes: [u8; 32]) -> Self {
            Self(bytes)
        }

        pub fn as_bytes(&self) -> &[u8; 32] {
            &self.0
        }

        /// 写进配置的样子：64 个小写十六进制字符。
        pub fn to_hex(&self) -> String {
            let mut out = String::with_capacity(KEY_HEX_LEN);
            for b in self.0 {
                use std::fmt::Write;
                let _ = write!(out, "{b:02x}");
            }
            out
        }
    }

    fn hex_digit(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }

    /// **不打印出来。**钥匙会跟着别的结构体一起落进 `tracing` 的 Debug 输出，
    /// 而日志是会被整段贴进 issue 的。
    impl std::fmt::Debug for ControlKey {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("ControlKey(<redacted>)")
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// 两个仓库拼出来的必须是同一个位置 —— 这个函数存在的全部理由。
        #[test]
        fn the_endpoint_lands_in_the_data_directory() {
            let d = Path::new("/data");
            match Address::in_dir(d) {
                Address::Socket(p) => assert_eq!(p, d.join(SOCKET_FILE)),
                Address::Loopback { port_file } => assert_eq!(port_file, d.join(PORT_FILE)),
            }
            assert_eq!(config_file(d), d.join(CONFIG_FILE));
        }

        #[test]
        fn a_key_is_exactly_64_hex_characters() {
            let hex = "9f2c".repeat(16);
            let k = ControlKey::parse(&hex).unwrap();
            assert_eq!(k.to_hex(), hex);
            assert_eq!(k.as_bytes()[0], 0x9f);
            // 大写也认，写回去是小写；首尾空白不算
            let loud = format!("  {}  ", hex.to_uppercase());
            assert_eq!(ControlKey::parse(&loud).unwrap(), k);
            assert_eq!(
                ControlKey::parse(&hex[..62]),
                Err(KeyFormatError::Length { len: 62 })
            );
            assert_eq!(
                ControlKey::parse(&format!("{hex}00")),
                Err(KeyFormatError::Length { len: 66 })
            );
            assert_eq!(
                ControlKey::parse(""),
                Err(KeyFormatError::Length { len: 0 })
            );
            let bad = format!("{}zz", &hex[..62]);
            assert_eq!(ControlKey::parse(&bad), Err(KeyFormatError::NotHex));
            // 多字节字符：字节数凑得上 64，字符数凑不上
            let wide = format!("{}\u{e9}", &hex[..62]);
            assert_eq!(wide.len(), 64);
            assert!(ControlKey::parse(&wide).is_err());
        }

        /// 打码用的那一串长度和真钥匙一样，但它本身不是一把钥匙。
        #[test]
        fn the_mask_is_as_long_as_a_key_and_is_not_one() {
            assert_eq!(KEY_MASK.len(), KEY_HEX_LEN);
            assert!(ControlKey::parse(KEY_MASK).is_err());
        }

        #[test]
        fn a_key_does_not_print_itself() {
            let k = ControlKey::from_bytes([0xab; 32]);
            assert!(!format!("{k:?}").contains("abab"));
        }

        /// 平台决定用哪一档，不是调用方挑。
        #[test]
        fn each_platform_gets_the_only_transport_it_has() {
            let got = Address::in_dir(Path::new("/data"));
            if cfg!(unix) {
                assert!(matches!(got, Address::Socket(_)));
            } else {
                assert!(matches!(got, Address::Loopback { .. }));
            }
        }
    }
}
