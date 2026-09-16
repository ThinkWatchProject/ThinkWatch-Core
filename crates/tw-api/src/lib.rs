//! 控制面的**契约**。CLI、Tauri UI、第三方都依赖它。
//!
//! 它刻意不含任何 IO —— 只有类型。这样它能被 Tauri 的前端（通过
//! ts-rs 之类的导出）、CLI、和将来的第三方同时依赖，而不会拖上一个
//! HTTP 栈。

use serde::{Deserialize, Serialize};

/// 控制面协议版本。UI 和 CLI 连上来时检查，不匹配就明确提示「请升级
/// 客户端」，而不是以奇怪的方式失败。
pub const CONTROL_API_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub api_version: u32,
    /// 二进制的 CalVer
    pub version: String,
    pub pid: u32,
    /// 数据面在监听哪儿。安全模式下是 None —— 那正是「只起控制面」的
    /// 可观测形态。
    pub gateway_addr: Option<String>,
    pub config_path: String,
    pub clients: usize,
    pub providers: usize,
    pub uptime_secs: u64,
}

/// 一次请求的观测事件。UI 的实时列表吃这个。
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        /// 这次请求属于哪一段对话的指纹。
        ///
        /// **只是指纹，不是会话 id** —— 会话是「同一个指纹 + 没隔太久」，
        /// 而「隔了多久」要看上一条是什么时候，那是 recorder 的活。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_fp: Option<String>,
        provider: String,
        /// 客户端要的模型名。**成本要靠它查价**，而它只在请求体里 ——
        /// 少了这个字段，落库那一步就只能记一笔没有模型的账
        #[serde(default)]
        model: String,
        method: String,
        path: String,
        at_ms: u64,
    },
    /// 收到上游响应头。**这个事件单独存在是有意的**：流式请求从这里
    /// 到结束可能还有好几分钟，UI 要能在这个点就把行画出来并显示
    /// 「进行中」，而不是等它结束才出现。
    RequestHeaders { id: u64, status: u16, ttfb_ms: u64 },
    /// 结束了
    RequestFinished {
        id: u64,
        status: u16,
        bytes: u64,
        duration_ms: u64,
        /// 上游报的用量。**没报就是 None，不是零** —— 零会让一次真实的
        /// 调用看起来是免费的
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<UsageView>,
    },
    /// 失败了。`source` 和 HTTP 响应里的 `x-thinkwatch-error` 是同一个词表。
    RequestFailed {
        id: u64,
        source: String,
        message: String,
    },
    /// 路由决定完了，尝试链也走完了。
    ///
    /// **单独一个事件，因为成功和失败两条路都要发它。**挂在
    /// `RequestFinished` 上的话，失败的那条路就没有尝试链 —— 而那恰恰
    /// 是最需要看它的时候。
    RequestRouted {
        id: u64,
        /// 命中了哪条规则。**日志和界面都要显示它** —— 「命中第 4 条」
        /// 远不如「命中『带缓存的必须走官方』」有用
        rule: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<String>,
        /// 试过哪几家、各自什么结果。**一次就成的也有一条** ——
        /// 「只试了一家」和「试了三家」在用户眼里应该是不同的
        attempts: Vec<AttemptView>,
        /// 最终服务的那家怎么收钱。
        ///
        /// **必须跟着这次请求走，不能事后查配置** —— 配置随时会被热重载，
        /// 而一条三天前的记录该按它当时那家的计费方式算。
        #[serde(default)]
        billing: String,
    },
    /// 一个请求体里带着看起来像凭据的东西（观察态）。
    ///
    /// **只记录，不改变任何行为。**换成占位符是「拦截」态的事，而那要
    /// 等那套完整的脱敏。
    LeakSeen {
        id: u64,
        provider: String,
        /// 「Anthropic API key」这类人话。字段叫 `secret` 而不是 `kind`
        /// —— 那个名字已经被枚举的 tag 占了（`probe` 那次同样的坑）
        secret: String,
        /// **已打码**。报出来的东西一律打码 —— 「发现了 sk-ant-xxx」
        /// 这句话本身就是一次泄漏
        masked: String,
        at_ms: u64,
    },
    /// 出站脱敏动手了。
    ///
    /// **界面上必须能看到脱敏发生了什么** —— 看不见的安全功能会被用户
    /// 关掉，因为他们会怀疑是脱敏搞坏了功能。
    Redacted {
        id: u64,
        provider: String,
        items: Vec<RedactedItem>,
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
        detail: String,
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
        from: String,
        to: String,
        dropped: Vec<String>,
        at_ms: u64,
    },
    /// 这条响应长什么样（防线三）。
    ///
    /// **只有形状，没有内容**：几个工具调用、命中几条规则。攒起来就是
    /// 每个上游的行为画像 —— 一个用了三个月一直正常的中转站，某天开始
    /// 返回大量 bash 调用，那是统计异常。
    ///
    /// 单独一个事件而不是挂在 `RequestFinished` 上：它只在开了入站审查
    /// 时才有，而 `RequestFinished` 是每条请求都有的。
    ResponseInspected {
        id: u64,
        tool_calls: u32,
        flagged: u32,
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
        rule: String,
        why: String,
        /// 命中的那一小段，**已截断**
        excerpt: String,
        high: bool,
        /// 真的切断了流吗。**高危 + 不受信任 + 拦截态**三者同时成立才会
        blocked: bool,
        at_ms: u64,
    },
    /// 客户端配置面上**新出现**了可疑的东西。
    ///
    /// **只报新出现的那些。**「一个用了半年的 skill 突然多了一段零宽
    /// 字符」这个信号，比「这个文件里有可疑内容」强得多 —— 而后者在
    /// 用户第一次打开页面时就已经全部看过了。
    ///
    /// 字段叫 `alerts` 而不是 `findings`，是为了和「打开页面扫一次」
    /// 那份完整清单区分开：这里的每一条都值得打断用户一次。
    ScanAlert {
        id: u64,
        alerts: Vec<ScanFinding>,
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
    /// 客户端配置面上的文件动了 —— **不管改了什么**。
    ///
    /// 和 `ScanAlert` 是两件事。那条说的是「出现了可疑内容」，值得打断
    /// 用户；这条只说「磁盘上那几个文件变了」，界面据此重读一遍接管
    /// 状态。用户在编辑器里把 `ANTHROPIC_BASE_URL` 改回原样，一点都不
    /// 可疑，但界面必须跟上 —— 没有这条，那一页只能每五秒重扫一次磁盘。
    ClientsChanged { id: u64, at_ms: u64 },
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
        state: String,
        at_ms: u64,
    },
    /// 上游在响应头里报了订阅额度。
    ///
    /// **零成本**：不发额外请求，顺着真实流量白捡。按量付费的账号没有
    /// 这些头，那时这个事件根本不会出现 —— 而不是报一个「用了 0%」。
    QuotaSeen {
        id: u64,
        provider: String,
        windows: Vec<QuotaWindow>,
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
        /// 「界面」「命令行」「外部编辑」「回滚」
        origin: String,
        at_ms: u64,
    },
    /// 新配置没过关，**旧的还在服务**。
    ///
    /// 桌面工具不能因为一个笔误就断线，所以这不是崩溃，是一条要展示给
    /// 人看的信息 —— 托盘变黄、界面标红、定位到那一行。
    ConfigRejected {
        id: u64,
        /// 「语法」「字段」「语义」
        stage: String,
        message: String,
        /// 1 起。语义错误没有，那时硬指一行只会误导
        line: Option<usize>,
        /// 出错那一行的原文，**已脱敏**
        excerpt: Option<String>,
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
        /// 「连通性检查」这类人话标签。字段叫 `probe` 而不是 `kind` ——
        /// 那个名字已经被枚举的 tag 占了
        probe: String,
        at_ms: u64,
    },
}

/// 尝试链里的一跳。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttemptView {
    pub provider: String,
    /// 「成功」「429 限流」「连不上上游」这类人话。**失败的原因要留着**
    /// —— 一条说「试过 A → B → C」的链，和一条还说清每一跳为什么失败的
    /// 链，排查价值差得远
    pub outcome: String,
    pub ms: u64,
}

/// 一次请求的路由决策。**详情抽屉的 Routing 那一页吃它。**
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RoutingView {
    pub rule: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub attempts: Vec<AttemptView>,
}

/// 一个订阅额度窗口。**每个字段都直接来自上游的响应头。**
///
/// 我们自己推断的东西不放进这个结构 —— 界面上必须能区分「上游说的」和
/// 「我们猜的」，而混在一个类型里就区分不了了。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub label: String,
    pub used_percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_in_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// 一次调用的用量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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
            | Event::LocallyAnswered { id, .. }
            | Event::ConfigReloaded { id, .. }
            | Event::ConfigRejected { id, .. }
            | Event::QuotaSeen { id, .. }
            | Event::LeakSeen { id, .. }
            | Event::ScanAlert { id, .. }
            | Event::RequestPriced { id, .. }
            | Event::ClientsChanged { id, .. }
            | Event::HealthChanged { id, .. }
            | Event::Redacted { id, .. }
            | Event::ToolCallFlagged { id, .. }
            | Event::ResponseInspected { id, .. }
            | Event::Translated { id, .. }
            | Event::CredentialRotated { id, .. }
            | Event::RequestRouted { id, .. } => *id,
        }
    }
}

/// 界面要显示的配置概览。
///
/// **不是配置文件本身**：密钥一律只给来源描述，不给值（统一脱敏）。
/// 界面需要的是「有哪些上游、规则怎么写的、谁健康」，不是那份 YAML。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Overview {
    pub providers: Vec<ProviderView>,
    /// 配置里定义过的代理。**界面上换代理要从这里选** —— 让用户
    /// 手打一个名字，打错了就是一次静默的「配了没生效」
    #[serde(default)]
    pub proxies: Vec<ProxyView>,
    pub routes: Vec<RouteView>,
    pub groups: Vec<GroupView>,
    pub clients: Vec<ClientView>,
    pub listen: ListenView,
    /// 三条防线各自的状态。
    ///
    /// **界面要能配它们，而不只是显示。**在此之前这三个字段根本没出现
    /// 在这个视图里，于是「脱敏开没开」只能去翻 config.yaml —— 而三态
    /// 的整个设计前提是「出厂停在观察态，用户看到证据之后自己决定要不
    /// 要切到拦截」，一个切不了的开关让那个设计不成立。
    #[serde(default)]
    pub security: SecurityView,
    /// 没绑路由的密钥走哪条
    #[serde(default)]
    pub default_route: String,
    /// 客户端自己发的辅助请求怎么处理
    #[serde(default)]
    pub client_probes: Vec<ProbeView>,
    /// 并发上限
    #[serde(default)]
    pub limits: LimitsView,
}

/// 一个出站代理。
///
/// **密码不在这里。**`ProxyAuth.pass` 和上游的 key 是同一类东西 ——
/// 这个视图会进日志、进诊断包、进用户贴出来的截图。有没有认证是要
/// 显示的，认证内容不是。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyView {
    pub name: String,
    /// `http` / `socks5`
    pub kind: String,
    pub addr: String,
    pub has_auth: bool,
    /// 有几家上游在用它。删之前要知道
    pub used_by: usize,
}

/// 一类客户端辅助请求的处置。
///
/// **这一段以前在界面上完全不存在，而它的缺席是连锁的**：路由条件
/// `when.intent` 只有在对应那一类被配成 `route` 时才可能命中，所以
/// 界面上那些写了 `intent` 的规则永远不会生效，而用户无从知道为什么。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeView {
    /// `health_check` / `warmup` / `titling` / `topic_detect` / `suggestion`
    pub id: String,
    /// 中文名
    pub label: String,
    /// 这一类是什么请求，一句话
    pub what: String,
    /// `intercept` / `route` / `passthrough`
    pub mode: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LimitsView {
    /// 全局并发
    pub max_concurrent: usize,
    /// 单个上游
    pub per_provider: usize,
    /// 队列上限。满了才真的拒绝
    pub queue_depth: usize,
    /// 排太久还是要放弃
    pub queue_timeout_secs: u64,
}

/// 三条防线。每条三态，而**「拦截」在每条上做的事不一样**，所以动词也
/// 一起给出来 —— 界面上统一叫「拦截」的话，用户点下去并不知道会发生
/// 什么。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecurityView {
    /// 出站脱敏。拦截态 = 替换成占位符
    pub redact: String,
    /// 入站审查。拦截态 = 切断响应流
    pub inspect_tools: String,
    /// 配置面扫描。拦截态 = 告警（它本来就不删东西）
    pub scan_configs: String,
    /// 用户加了几条自定义扫描规则、停用了几条内置的
    pub scan_rules_added: usize,
    pub scan_rules_disabled: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderView {
    pub name: String,
    /// 已脱敏
    pub base_url: String,
    /// 密钥的**来源**，不是值
    pub key_source: String,
    pub protocol: Option<String>,
    pub proxy: String,
    /// closed / open
    pub health: String,
    /// 这家怎么收钱。`cheapest` 策略和成本栏都看它
    #[serde(default)]
    pub billing: Option<String>,
    /// 可不可信。**不写就按 base_url 判**，这里给的是判完的结果
    #[serde(default)]
    pub trust: String,
    /// 用户有没有在配置里显式写过 `trust`。
    ///
    /// **界面要能区分「自动判成不受信任」和「用户写了不受信任」** ——
    /// 前者改 base_url 就会变，后者不会，而两者显示成一样会让用户
    /// 以为自己改不动它。
    #[serde(default)]
    pub trust_explicit: bool,
    /// 这家上游实际会脱哪几类。
    ///
    /// 给的是**判完的结果**，不是配置里写的那几个字 —— 不写的话官方端点
    /// 是空的、其余是那四类默认。界面上要能看出「没写」和「写了空」的
    /// 区别，所以另给一个 explicit 标志。
    #[serde(default)]
    pub redact: Vec<String>,
    #[serde(default)]
    pub redact_explicit: bool,
}

/// 一条规则。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleView {
    pub name: String,
    pub to: String,
    /// `when` 的人话摘要。空 = 兜底
    pub conditions: Vec<String>,
}

/// 一条路由 —— 一组规则，加上它分给了谁。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteView {
    pub name: String,
    /// 没绑路由的密钥走的就是这条
    pub default: bool,
    /// **显式绑了这条路由的密钥。**默认路由这里通常是空的 —— 走它的人
    /// 是「没绑」，不是「绑了它」，而把所有密钥列进来会让人以为那是
    /// 一次次显式的选择。
    pub clients: Vec<String>,
    pub rules: Vec<RuleView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupView {
    pub name: String,
    pub kind: String,
    /// 同一次会话固定走同一家。**这一项直接决定账单**
    #[serde(default)]
    pub session_affinity: bool,
    /// `select` 组当前选中谁。
    ///
    /// **界面要能切它** —— 这个策略本身就是「UI 上点选或托盘里切」，
    /// 而切不了的话它等于一个只能改 YAML 才能用的功能。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
    pub providers: Vec<String>,
    /// 这个策略会不会让 prompt cache 不稳定。**要在界面上直说** ——
    /// 它决定了用户的账单。
    pub hurts_cache: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientView {
    pub name: String,
    /// 已脱敏
    pub key: String,
    pub max_concurrent: Option<usize>,
    /// 绑的那条路由。`None` = 走默认路由
    #[serde(default)]
    pub route: Option<String>,
    /// 这把密钥能看到哪些模型。三态：不写 / 写非空 / 写 `[]`（一个都不给）
    #[serde(default)]
    pub allow: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListenView {
    pub bind: String,
    pub port: u16,
    /// 实际生效的白名单（`lan`/`all` 下会是默认填的私网段）
    pub allow_from: Vec<String>,
    /// 非 loopback 时为真。界面上要据此把「关闭密钥校验」置灰
    pub exposed: bool,
}

/// 探一个上游能不能用。零成本，见L1/L2。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeRequest {
    pub base_url: String,
    pub key: String,
}

/// 模型清单的结果。**空列表不足以表达**：「上游没这个接口」「上游给了
/// 但我们没认出格式」「真的一个都没有」是三件事，塌成空列表之后 UI 只能
/// 说「这家不提供模型列表」，而那在第二种情况下是编的 —— 把我们自己的
/// 解析缺口说成了对方的特性。
#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResponse {
    pub ok: bool,
    pub protocol: Option<String>,
    pub latency_ms: u64,
    pub models: ModelList,
    pub error: Option<String>,
}

/// L1 测速：只握手，不发业务请求。**零成本零副作用**，可以随便点。
///
/// 三种问法，但它们不是三个概念：
///
/// - `provider: Some(名字)` —— 测一个已配置的上游，代理按它自己的配置走
/// - `proxy: Some(名字)` —— 只测代理本身。**代理测速就到这一层为止**，
///   代理影响的是网络层，没有理由为了测代理去调用模型
/// - `base_url: Some(地址)` —— 还没保存时用，首次配置那一步
///
/// 全不给就测所有上游。L1 零成本，批量不需要确认（L3 才需要）。
///
/// **不接受一个「候选 URL 列表」。** cc-switch 有那么一张表，测完还得手动
/// 点一下填进去，运行时永远只认当前保存的那一个 —— 同一个概念在一个程序
/// 里存在两次，两边不通。这里的规矩是：测的候选池就是运行时故障转移的
/// 候选池，同一份数据（架构红线）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct L1Request {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct L1Segment {
    pub name: String,
    pub ms: u64,
}

/// **分段是个列表而不是固定的 DNS/TCP/TLS 三段**，因为走代理时的形状本来
/// 就不同：多出「代理握手」，而 `socks5h` 下根本没有本地 DNS 那一段。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L1Result {
    /// 实际测的是什么 —— 回显出来，别让用户猜点的那一下测了谁
    pub target: String,
    /// 经过哪个代理，直连是 None
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    pub ok: bool,
    pub segments: Vec<L1Segment>,
    pub total_ms: u64,
    /// 解释为什么某一段不在上面。**没有这句话，缺一段看起来就像 bug。**
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 当前的配置文本，连同它的版本号。
///
/// **给的是原文，不是结构。**界面的文本模式直接显示它；表单模式改完
/// 之后带着 `version` 回来，那就是乐观并发的凭据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigText {
    pub path: String,
    pub text: String,
    /// `blake3:xxxxxxxxxxxx`，和 `PATCH` 的 `base_version` 是同一个
    pub version: String,
}

/// 一把刚生成、还没写进配置的网关密钥（`GET /keys/new`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewKey {
    pub key: String,
}

/// 这台机器上的一张网卡（`GET /interfaces`）。
///
/// **界面上「绑在哪张网卡」那个选单要的就是它。**没有它，用户只能自己
/// 去 `ifconfig` 抄一个地址填进配置文件，而填错的后果是网关起不来。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NicView {
    /// `en0`、`lo0`、`utun3`
    pub name: String,
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
#[serde(untagged)]
pub enum PatchValue {
    Str(String),
    Int(i64),
    Bool(bool),
    /// 显式的空。`null` 和空字符串是两回事
    Null,
}

/// 「检查价格更新」第一步：**先说要访问什么、多大**。
///
/// **绝不在启动时后台偷偷拉。**零上传那句承诺，也意味着零静默下载 ——
/// 而「先告诉你要连哪儿」是这条承诺里最容易被省掉的一半。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateOffer {
    pub url: String,
    /// 字节。`None` = 对面没给 `Content-Length`
    pub bytes: Option<u64>,
    /// 现在这份快照是哪天的
    pub current_date: String,
}

/// 第二步：下载完、解析完，**给 diff，还没写**。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatePreview {
    /// 拉回来的表里有多少个带价的模型
    pub models: usize,
    /// 真的变了的那些。**不列没动的** —— 三千多个里绝大多数没动，
    /// 一起列出来等于把那几十条真的变化埋掉
    pub changes: Vec<PriceChangeView>,
    /// 这份东西的指纹。第三步要带着它回来 —— 否则「确认写入」写的
    /// 可能是另一次下载的结果
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceChangeView {
    pub model: String,
    /// `null` = 新增的
    pub old_input: Option<f64>,
    pub new_input: f64,
    pub old_output: Option<f64>,
    pub new_output: f64,
}

/// 一条用户自己写的价格（第三层）。
///
/// **单位是每百万 token 的美元**，和厂商定价页上印的一样 —— 让用户
/// 在界面上填 `0.000003` 是在要求他做一次换算，而换算是会错的。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceRow {
    /// 哪个上游。`None` = 对所有上游生效
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub model: String,
    /// 每百万 token 的美元
    pub input: f64,
    pub output: f64,
    /// 内置快照里有没有这个模型。**界面要能说「这条是在覆盖」**，
    /// 因为覆盖一个本来就有价的模型，和补一个没价的，是两件事
    #[serde(default)]
    pub overrides_builtin: bool,
}

/// 用户的价格覆盖，连同「内置那份是什么时候的」。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricingView {
    pub rows: Vec<PriceRow>,
    pub snapshot_date: String,
    /// 最近这段时间里**算不出价钱**的请求数。
    ///
    /// **这是这一页存在的理由** —— 用户不会主动想起要配价格，只有
    /// 「有 37 条请求算不出钱」这种具体证据才会（触发条件）。
    pub unpriced_recent: i64,
    /// 那些算不出价钱的请求用的是哪些模型。**直接告诉他要填什么**
    pub unpriced_models: Vec<String>,
}

/// 光标落在配置的哪一段上。
#[derive(Debug, Clone, Serialize, Deserialize)]
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
pub struct ConfigWrite {
    pub base_version: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigWritten {
    pub version: String,
}

/// 历史里的一版。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigVersion {
    pub version: String,
    pub at_ms: u64,
    /// 「界面」「命令行」「外部编辑」「回滚」
    pub origin: String,
    pub bytes: u64,
    /// 这一版是现在跑着的那一版吗。
    ///
    /// **历史里包括当前版本**，所以列表最上面那条通常就是它 —— 不标
    /// 出来的话，用户会以为第一条是「上一版」然后回滚到自己身上。
    #[serde(default)]
    pub current: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
pub struct CostBucket {
    /// 桶的起点
    pub at_ms: i64,
    pub requests: i64,
    pub failed: i64,
    pub cost_micros_exact: i64,
    pub cost_micros_estimated: i64,
    pub unpriced_requests: i64,
}

/// 一个时间桶里，某一个模型（或上游）的那部分。
///
/// **和 `CostBucket` 是两个查询，不是一个的扩展。**趋势图要回答的是
/// 「什么时候花的」和「花在哪个模型上」—— 合成一张按模型分层的图之后，
/// 这两个问题只用看一次；而把它们拆成一张趋势图加一张构成图，读的人
/// 要在两张图之间自己对时间。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostBucketGroup {
    pub at_ms: i64,
    /// 模型名或上游名，看查的是哪一维
    pub name: String,
    pub requests: i64,
    pub failed: i64,
    pub cost_micros_exact: i64,
    pub cost_micros_estimated: i64,
    /// 这一格里这一项用掉的 token。
    ///
    /// **四类分开给。**它们的单价差十倍以上，加成一个数之后既算不回
    /// 钱，也说不清「这段时间是在写新上下文还是在吃缓存」。
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub cache_read_tokens: i64,
    #[serde(default)]
    pub cache_write_tokens: i64,
}

/// 按模型或上游分组的花费（钱花在哪儿）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostGroup {
    pub name: String,
    pub requests: i64,
    pub cost_micros: i64,
    pub unpriced_requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// 分组维度。**是个枚举不是字符串** —— 它最终来自 query string，
/// 而把它拼进 SQL 的列名里就是一个注入口。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CostDim {
    Model,
    Provider,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// 有多少条请求根本没有价格。**不是 0，是「不知道」**
    pub unpriced_requests: i64,
    /// 走订阅型上游的请求数。**不参与金额合计** ——
    /// 订阅制的边际成本是零，按价目表算出来的数字是纯虚构的
    #[serde(default)]
    pub subscription_requests: i64,
    /// 那些请求用掉的 token。**它才是订阅用户该看的量**
    #[serde(default)]
    pub subscription_tokens: i64,
    /// 用了缓存之后净省下多少微分。
    ///
    /// **净额：命中节省的部分，减去写入产生的溢价。**用户想知道的是
    /// 「如果完全不用缓存，这段时间要多花还是少花」—— 而缓存写入按
    /// 1.25 倍单价计费，所以这个数可以是负的。
    #[serde(default)]
    pub cache_saved_micros: i64,
    /// 本区间有多少个请求带回了可疑工具调用（防线三）
    #[serde(default)]
    pub flagged_requests: i64,
    /// 本区间有多少个请求在出站时被脱敏换过内容（防线一的拦截档）
    ///
    /// **观察档不产生这个数**，它产生的是 `/leaks` 里那些证据。两档
    /// 各有各的痕迹，界面上要分别说明 —— 否则切到拦截之后看起来像
    /// 什么都没发生，而那是防护更强的一档。
    #[serde(default)]
    pub redacted_requests: i64,
    /// 价目表的快照日期。**成本旁边要标它** —— 一个两个月前
    /// 的价目表算出来的数字，可信度和昨天的完全不同
    pub pricing_date: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyView {
    pub model: String,
    pub p50: i64,
    pub p95: i64,
    /// **样本数要一起给。**「800ms」是 3 个样本还是 300 个，含义完全不同
    pub samples: usize,
}

/// 一条历史请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub error: Option<String>,
    /// 本地应答的
    pub local: bool,
    /// 服务它的那家怎么收钱：`per-token` / `subscription` / `unknown`
    #[serde(default)]
    pub billing: String,
    /// 缓存命中省下了多少微分。`None` = 算不出来
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_saved_micros: Option<i64>,
    /// 路由决策与尝试链。老记录没有它
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<RoutingView>,
}

/// 一条请求的全部细节。**详情抽屉吃这个。**
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestDetail {
    pub row: HistoryRow,
    pub request_body: Option<BodyView>,
    pub response_body: Option<BodyView>,
}

/// 一份存下来的 body。
#[derive(Debug, Clone, Serialize, Deserialize)]
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
pub struct ProviderQuota {
    pub provider: String,
    pub windows: Vec<QuotaWindow>,
}

/// L3 测速要花多少。
///
/// **这是「你确认要花钱吗」那个对话框的全部内容。**触发前必须显示它，
/// 而不是点了才知道。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeedEstimate {
    pub provider: String,
    pub model: String,
    /// 输入 token。**精确值** —— 请求是固定的
    pub input_tokens: u64,
    pub max_output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<i64>,
    /// 给人看的那一句
    pub note: String,
}

/// 一批测速的账。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeedQuote {
    pub items: Vec<SpeedEstimate>,
    /// 总计。**有一项算不出来就是 None** —— 给一个看起来完整的数字，
    /// 用户会以为那就是全部代价
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_micros: Option<i64>,
    pub pricing_date: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpeedRunRequest {
    /// 不给就是所有上游
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// **必填。**同一个 provider 的 Opus 和 Haiku 是两条完全不同的曲线，
    /// 不指定模型的测速结果没有意义
    pub model: String,
}

/// 一次 L3 测速的结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub error: Option<String>,
}

/// 「过去 7 天，有 3 个请求把你的 API key 发给了 relay-cn」。
///
/// **这比任何功能介绍都有说服力**，因为它说的是已经发生在你身上的事。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeakGroup {
    pub provider: String,
    pub kind: String,
    pub requests: i64,
    pub last_at_ms: i64,
    /// 涉及哪几把，**都已打码**
    pub masked: Vec<String>,
}

/// 观测这一层现在能不能写。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageStatus {
    /// 「正常」/「磁盘快满了…」/「磁盘几乎满了…」
    pub level: String,
    /// 记了多少条
    pub rows: i64,
    /// 请求体占了多少字节
    pub blob_bytes: u64,
    /// **转发受影响了吗。永远是 false** —— 观测挂了，代理照跑
    pub forwarding_affected: bool,
}

/// 首次运行时写下第一个上游。
///
/// **只在还没有 provider 时可用**。之后改配置走双向同步（M2），
/// 那是另一套机制：这里是从无到有整文件生成，那边是改一个字节而保住
/// 其余全部。混用会让「注释和格式原样保留」这条承诺失效。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupRequest {
    pub name: String,
    pub base_url: String,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupResponse {
    /// 写完之后，客户端该用哪把网关密钥
    pub gateway_key: String,
    pub gateway_addr: String,
    pub config_path: String,
}

/// 换掉了哪一类、几处。**只有类别和计数，没有原值。**
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactedItem {
    /// `api-keys` 这类机器读的标记
    pub kind: String,
    /// 「Anthropic API key」这类人话
    pub what: String,
    pub count: u64,
}

// ---------------------------------------------------------------- 会话

/// 一次任务。
///
/// **孤立地看单个请求，看不出任何有用的东西** —— Claude Code 的一次任务
/// 是几十到上百个请求，携带不断增长的上下文。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    pub id: String,
    pub client: String,
    pub started_ms: u64,
    pub ended_ms: u64,
    pub turns: u64,
    /// 有价格的那些轮次加起来，单位是**微分**
    pub cost_micros: i64,
    /// **没有价格的轮数。**「$1.23」和「$1.23，另有 4 轮没有价格」是两个
    /// 不同的结论
    pub unpriced_turns: u64,
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
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDetail {
    pub session: SessionView,
    pub turns: Vec<TurnView>,
}

// ---------------------------------------------------------------- 请求重放

/// 把存下来的那条请求，原样发给另一个上游。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayRequest {
    pub id: i64,
    pub provider: String,
}

/// 报价。**按下确认之前必须看到它**（和 L3 测速同一条纪律）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayQuote {
    pub model: String,
    pub provider: String,
    pub body_bytes: i64,
    pub input_tokens: i64,
    /// `None` = 订阅型，或者这个模型不在价目表里。**不是 0**
    pub cost_micros: Option<i64>,
    pub note: String,
    /// 发出去之前会不会脱敏。用户有权在按下去之前知道
    pub will_redact: bool,
    pub pricing_date: String,
}

/// 原来那一次长什么样，用来并排比。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayOriginal {
    pub provider: String,
    pub status: Option<u16>,
    pub ttfb_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    pub bytes: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

// ---------------------------------------------------------- 上游行为基线

/// 一个上游最近是不是变了（防线三）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftView {
    /// `tool_calls` / `flagged` / `errors`
    pub metric: String,
    pub label: String,
    /// 比率，0..1
    pub recent: f64,
    pub baseline: f64,
    /// 两边各自的样本量。**必须一起显示** —— 没有它，比率是个没法判断
    /// 可信度的数字
    pub recent_n: i64,
    pub baseline_n: i64,
    pub notable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderBaseline {
    pub provider: String,
    /// 最近这一段有多少条请求
    pub recent_total: i64,
    /// 基线那一段有多少条
    pub baseline_total: i64,
    /// 数过形状的有多少条。**和总数不同时要说** —— 关掉入站审查的那段
    /// 时间没有数过，画像里不该假装它们是「没有工具调用」
    pub recent_inspected: i64,
    pub baseline_inspected: i64,
    pub drifts: Vec<DriftView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineResponse {
    /// 最近这一段有多长（小时）
    pub recent_hours: u32,
    /// 基线那一段有多长（天）
    pub baseline_days: u32,
    pub providers: Vec<ProviderBaseline>,
    /// 观测层没起来时是 true，界面上要说清「不是没发现，是没看」
    pub unavailable: bool,
}

// ---------------------------------------------------------------- 静态扫描

/// 一处发现。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanFinding {
    /// `high` | `medium` | `low`
    pub level: String,
    /// 哪条规则命中的
    pub rule: String,
    /// `hooks` | `mcp` | `skill` | `command` | `agent` | `instructions`
    pub kind: String,
    pub kind_label: String,
    pub client: String,
    pub path: String,
    /// 第几行，从 1 开始
    pub line: usize,
    pub title: String,
    pub detail: String,
    /// 命中的那一行，**不可见字符已经换成可见记号**
    pub excerpt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpView {
    pub name: String,
    pub client: String,
    pub command: String,
    pub args: Vec<String>,
    /// 远端型的地址
    pub url: Option<String>,
    /// **只有名字，没有值**
    pub env_keys: Vec<String>,
    pub enabled: bool,
    pub source: String,
    /// 远端而且不在本机
    pub third_party: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillView {
    pub name: String,
    pub client: String,
    pub path: String,
    pub allowed_tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookView {
    pub client: String,
    pub event: String,
    pub command: String,
    pub source: String,
}

/// 扫一次的结果。
///
/// **不存任何东西**：这是此刻磁盘上的真实情况，页面关了就没了。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResponse {
    pub findings: Vec<ScanFinding>,
    pub mcp: Vec<McpView>,
    pub skills: Vec<SkillView>,
    pub hooks: Vec<HookView>,
    /// 同名但配置不同的 MCP server 名字（矩阵上要标记号）
    pub conflicting: Vec<String>,
    /// 读不动的文件。**要显示** —— 悄悄跳过会给人「查过了」的错觉
    pub unreadable: Vec<String>,
    pub scanned: usize,
    /// 规则从哪儿来的
    pub rules_origin: String,
    /// 用户的规则文件有问题时的那句话
    pub rules_warning: Option<String>,
    /// 这次连哪些项目目录一起扫了
    pub projects: Vec<String>,
}

/// 在矩阵上点一下。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpOpRequest {
    /// `copy` 或 `remove`
    pub op: String,
    pub name: String,
    /// `copy` 时从哪个客户端取
    #[serde(default)]
    pub from: Option<String>,
    /// 写到（或从中删掉）哪个客户端
    pub to: String,
}

/// 哪些客户端能被写入，哪些只能看。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTargetView {
    pub client: String,
    pub name: String,
    pub path: String,
    /// 能不能往里写。**不能写的照样在清单里** —— 看得见是第一目标
    pub copyable: bool,
    /// 不能写的话，为什么
    pub why_not: String,
}

// ---------------------------------------------------------------- 路由试算

/// 「如果现在来这样一个请求，会走到哪儿」。
///
/// **每个字段都对应规则里能写的一个条件**。默认值就是一个最
/// 普通的请求 —— 用户只需要改他关心的那一两个。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DryRunRequest {
    pub model: String,
    /// 哪个客户端发的。空 = 用 config.yaml 里的第一个
    #[serde(default)]
    pub client: String,
    #[serde(default = "default_dialect")]
    pub dialect: String,
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

fn default_dialect() -> String {
    "anthropic".to_string()
}
fn default_true() -> bool {
    true
}

/// 一条规则在这次试算里的下场。**没命中的也要列出来，并说清为什么** ——
/// 「为什么没走我以为的那条」和「走了哪条」是同一个问题的两面。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleTrace {
    pub name: String,
    /// `matched` | `skipped` | `phase_two`
    pub verdict: String,
    /// 没命中时，是哪个条件没对上
    pub why: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DryRunResult {
    /// 这个组按什么挑（`按顺序` / `选最快` / `选最便宜` …）。
    ///
    /// **不说的话，用户看不懂候选为什么是这个顺序** —— 「我明明把官方
    /// 写在第一个」。直指 provider 时是 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    /// `route` | `deny` | `no_match`
    pub outcome: String,
    /// 命中的规则名
    pub rule: Option<String>,
    /// 拒绝的理由（`deny` 时）
    pub reason: Option<String>,
    /// 候选链，第一个是首选，后面是故障转移的备选
    pub candidates: Vec<String>,
    /// 经过了哪个组
    pub via_group: Option<String>,
    /// 累积起来的参数改写，人话形式
    pub set: Vec<String>,
    pub trace: Vec<RuleTrace>,
    /// 这条路会不会伤到 prompt cache。**要直说 —— 它决定账单**
    pub hurts_cache: bool,
    /// 候选链里此刻熔断着的那些。**试算是静态的，但熔断是当下的事实**
    pub circuit_open: Vec<String>,
}

// ---------------------------------------------------------- 客户端接管
//
// **注意 `DetectedClient` 和上面的 `ClientView` 是两个东西**：那个是
// config.yaml 里的一把网关密钥，这个是本机上装着的一个 AI 客户端 App。
// 中文都叫「客户端」，混起来的话，「有几个客户端」这句话就有两个答案。

/// 一个客户端此刻的样子。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedClient {
    pub id: String,
    pub name: String,
    /// 用户认得的那个路径
    pub path: String,
    /// 跟完符号链接的真身。**和 `path` 不同时要显示出来** —— 用户以为
    /// 在改 ~/.claude/settings.json，实际写的可能是他 dotfiles 仓库里
    /// 的那份，而那是个会被 git 提交的地方
    pub real: String,
    pub installed: bool,
    pub has_config: bool,
    pub adopted_at_ms: Option<u64>,
    /// 配置里此刻的端点。**读出来的**，不是拿我们自己的记录充数
    pub endpoint: Option<String>,
    pub shadows: Vec<String>,
    /// `immediately` | `on_restart`
    pub takes_effect: String,
    pub takes_effect_note: String,
    /// 接管之后要不要在「一直没收到请求」时提示。
    ///
    /// **需要重开终端的客户端不提示** —— 用户可能一整天都没重开过，那时
    /// 弹「是不是没生效」是狼来了
    pub warns_when_silent: bool,
    /// `measured` | `fields_only`
    pub verified: String,
    pub verified_note: String,
    pub costs: Vec<String>,
    /// 最后一次收到这个客户端的请求。**接管有没有真的生效，只有它能证明**
    pub last_seen_ms: Option<u64>,
}

/// 接管不了、只能给指引的。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManualClient {
    pub name: String,
    pub how: String,
    pub caveat: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientsResponse {
    pub clients: Vec<DetectedClient>,
    pub manual: Vec<ManualClient>,
    /// 客户端该连的地址
    pub gateway_base: String,
    /// config.yaml 里有哪几把网关密钥可选
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdoptRequest {
    pub client: String,
    /// 用哪把网关密钥。不写就用第一把 —— 为「一个 key 就够」的人设计
    #[serde(default)]
    pub key_name: Option<String>,
}

/// 算好但还没落盘的改动。**UI 拿它画 diff 让用户确认。**
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanView {
    pub client: String,
    pub path: String,
    /// 改之前的原文，**密钥已打码**。
    pub before: Option<String>,
    /// 改之后的原文，**密钥已打码 —— 落盘写的是真值**。
    ///
    /// 界面上永远不显示真正的密钥，diff 里也不行：用户会截图这一屏来问
    /// 「这样对吗」。
    pub after: String,
    pub notes: Vec<String>,
    pub shadows: Vec<String>,
    /// 已经是这样了，什么都不用改
    pub noop: bool,
    pub carries_secret: bool,
    /// 这次会把哪些字段改成什么，人话形式。diff 之外再给一份摘要
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdoptResponse {
    pub real: String,
    pub backup: String,
    pub created: bool,
    /// 不至于失败、但用户该知道的事（符号链接、权限太松……）
    pub warnings: Vec<String>,
    pub takes_effect_note: String,
}

/// 一条诊断发现。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindingView {
    /// `blocking` | `suspect` | `clear`
    pub level: String,
    pub title: String,
    pub detail: String,
    /// 用户可以自己执行的下一步。**我们不替他执行。**
    pub fix: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_carry_a_discriminating_tag() {
        // 前端按 `kind` 分派。少了它，TypeScript 那边只能靠字段有无来猜。
        let e = Event::RequestFinished {
            id: 1,
            status: 200,
            bytes: 10,
            duration_ms: 5,
            usage: None,
        };
        let v: serde_json::Value = serde_json::to_value(&e).unwrap();
        assert_eq!(v["kind"], "request_finished");
        assert_eq!(v["id"], 1);
    }

    #[test]
    fn every_event_exposes_its_request_id() {
        // UI 靠 id 把四个事件缝成一行。
        for e in [
            Event::RequestStarted {
                id: 7,
                client: "c".into(),
                client_hint: None,
                session_fp: None,
                provider: "p".into(),
                model: "m".into(),
                method: "POST".into(),
                path: "/v1/messages".into(),
                at_ms: 0,
            },
            Event::RequestHeaders {
                id: 7,
                status: 200,
                ttfb_ms: 1,
            },
            Event::RequestFinished {
                id: 7,
                status: 200,
                bytes: 1,
                duration_ms: 1,
                usage: None,
            },
            Event::RequestFailed {
                id: 7,
                source: "upstream".into(),
                message: "x".into(),
            },
        ] {
            assert_eq!(e.id(), 7);
        }
    }

    #[test]
    fn status_round_trips() {
        let s = Status {
            api_version: CONTROL_API_VERSION,
            version: "2026.9.0".into(),
            pid: 1,
            gateway_addr: Some("127.0.0.1:8788".into()),
            config_path: "/x/config.yaml".into(),
            clients: 1,
            providers: 1,
            uptime_secs: 0,
        };
        let back: Status = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.gateway_addr.as_deref(), Some("127.0.0.1:8788"));
    }

    #[test]
    fn safe_mode_is_representable() {
        // 安全模式 = 控制面在、数据面不在。它必须是一个能被表达的状态，
        // 而不是「gateway_addr 是空字符串」这种约定。
        let json = r#"{"api_version":1,"version":"x","pid":1,"gateway_addr":null,
                       "config_path":"/x","clients":0,"providers":0,"uptime_secs":0}"#;
        let s: Status = serde_json::from_str(json).unwrap();
        assert!(s.gateway_addr.is_none());
    }
}
