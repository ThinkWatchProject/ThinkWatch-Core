//! 控制面的**契约**。CLI、Tauri UI、第三方都依赖它（DESIGN.md §9.5）。
//!
//! 它刻意不含任何 IO —— 只有类型。这样它能被 Tauri 的前端（通过
//! ts-rs 之类的导出）、CLI、和将来的第三方同时依赖，而不会拖上一个
//! HTTP 栈。

use serde::{Deserialize, Serialize};

/// 控制面协议版本。UI 和 CLI 连上来时检查，不匹配就明确提示「请升级
/// 客户端」，而不是以奇怪的方式失败（§9.6）。
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
        /// 它存在的理由：§0.6 的目标用户「一个 key 就够」，那时五个客户端
        /// 的 `client` 是同一个值，而观察窗口要回答的偏偏是「Codex 那边
        /// 生效了吗」。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_hint: Option<String>,
        /// 这次请求属于哪一段对话的指纹（§7.9）。
        ///
        /// **只是指纹，不是会话 id** —— 会话是「同一个指纹 + 没隔太久」，
        /// 而「隔了多久」要看上一条是什么时候，那是 recorder 的活。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_fp: Option<String>,
        provider: String,
        /// 客户端要的模型名。**成本要靠它查价**，而它只在请求体里 ——
        /// 少了这个字段，落库那一步就只能记一笔没有模型的账（§4.3）
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
        /// 调用看起来是免费的（§4.3）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<UsageView>,
    },
    /// 失败了。`source` 和 HTTP 响应里的 `x-thinkwatch-error` 是同一个词表。
    RequestFailed {
        id: u64,
        source: String,
        message: String,
    },
    /// 路由决定完了，尝试链也走完了（§4.2）。
    ///
    /// **单独一个事件，因为成功和失败两条路都要发它。**挂在
    /// `RequestFinished` 上的话，失败的那条路就没有尝试链 —— 而那恰恰
    /// 是最需要看它的时候。
    RequestRouted {
        id: u64,
        /// 命中了哪条规则。**日志和界面都要显示它** —— 「命中第 4 条」
        /// 远不如「命中『带缓存的必须走官方』」有用（§3.4）
        rule: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<String>,
        /// 试过哪几家、各自什么结果。**一次就成的也有一条** ——
        /// 「只试了一家」和「试了三家」在用户眼里应该是不同的
        attempts: Vec<AttemptView>,
        /// 最终服务的那家怎么收钱（§4.3.1）。
        ///
        /// **必须跟着这次请求走，不能事后查配置** —— 配置随时会被热重载，
        /// 而一条三天前的记录该按它当时那家的计费方式算。
        #[serde(default)]
        billing: String,
    },
    /// 一个请求体里带着看起来像凭据的东西（§5.0 的观察态）。
    ///
    /// **只记录，不改变任何行为。**换成占位符是「拦截」态的事，而那要
    /// 等 §5.1 那套完整的脱敏。
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
    /// 出站脱敏动手了（§5.1）。
    ///
    /// **界面上必须能看到脱敏发生了什么** —— 看不见的安全功能会被用户
    /// 关掉，因为他们会怀疑是脱敏搞坏了功能。
    Redacted {
        id: u64,
        provider: String,
        items: Vec<RedactedItem>,
        at_ms: u64,
    },
    /// 这条响应长什么样（§5.2 防线三）。
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
    /// 上游返回的响应里有一个可疑的工具调用（§5.2）。
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
    /// 客户端配置面上**新出现**了可疑的东西（§5.3）。
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
    /// 上游在响应头里报了订阅额度（§4.3.2）。
    ///
    /// **零成本**：不发额外请求，顺着真实流量白捡。按量付费的账号没有
    /// 这些头，那时这个事件根本不会出现 —— 而不是报一个「用了 0%」。
    QuotaSeen {
        id: u64,
        provider: String,
        windows: Vec<QuotaWindow>,
        at_ms: u64,
    },
    /// 配置换了一份新的进去，已经生效（§3.8）。
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
    /// 新配置没过关，**旧的还在服务**（§3.8）。
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
    /// 客户端的辅助请求被本地应答了，一个字节都没发给上游（§4.8）。
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
    /// 链，排查价值差得远（§4.2）
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
/// 「我们猜的」，而混在一个类型里就区分不了了（§4.3.2）。
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
    /// 缓存写用的是 1 小时 TTL 吗。**差价接近一倍**（§4.3.0）
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
            | Event::Redacted { id, .. }
            | Event::ToolCallFlagged { id, .. }
            | Event::ResponseInspected { id, .. }
            | Event::RequestRouted { id, .. } => *id,
        }
    }
}

/// 界面要显示的配置概览。
///
/// **不是配置文件本身**：密钥一律只给来源描述，不给值（§9.7 的统一脱敏）。
/// 界面需要的是「有哪些上游、规则怎么写的、谁健康」，不是那份 YAML。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Overview {
    pub providers: Vec<ProviderView>,
    pub routes: Vec<RouteView>,
    pub groups: Vec<GroupView>,
    pub clients: Vec<ClientView>,
    pub listen: ListenView,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteView {
    pub name: String,
    pub to: String,
    /// `when` 的人话摘要。空 = 兜底
    pub conditions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupView {
    pub name: String,
    pub kind: String,
    pub providers: Vec<String>,
    /// 这个策略会不会让 prompt cache 不稳定。**要在界面上直说** ——
    /// 它决定了用户的账单（§3.4）。
    pub hurts_cache: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientView {
    pub name: String,
    /// 已脱敏
    pub key: String,
    pub max_concurrent: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListenView {
    pub bind: String,
    pub port: u16,
    /// 实际生效的白名单（`lan`/`all` 下会是默认填的私网段）
    pub allow_from: Vec<String>,
    /// 非 loopback 时为真。界面上要据此把「关闭密钥校验」置灰（§5.4）
    pub exposed: bool,
}

/// 探一个上游能不能用。零成本，见 §4.6 的 L1/L2。
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

/// L1 测速：只握手，不发业务请求（§4.6）。**零成本零副作用**，可以随便点。
///
/// 三种问法，但它们不是三个概念：
///
/// - `provider: Some(名字)` —— 测一个已配置的上游，代理按它自己的配置走
/// - `proxy: Some(名字)` —— 只测代理本身。§4.6：**代理测速就到这一层为止**，
///   代理影响的是网络层，没有理由为了测代理去调用模型
/// - `base_url: Some(地址)` —— 还没保存时用，首次配置那一步
///
/// 全不给就测所有上游。L1 零成本，批量不需要确认（L3 才需要，见 §4.6）。
///
/// **不接受一个「候选 URL 列表」。** cc-switch 有那么一张表，测完还得手动
/// 点一下填进去，运行时永远只认当前保存的那一个 —— 同一个概念在一个程序
/// 里存在两次，两边不通。这里的规矩是：测的候选池就是运行时故障转移的
/// 候选池，同一份数据（§4.6 的架构红线）。
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
/// 之后带着 `version` 回来，那就是乐观并发的凭据（§3.8）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigText {
    pub path: String,
    pub text: String,
    /// `blake3:xxxxxxxxxxxx`，和 `PATCH` 的 `base_version` 是同一个
    pub version: String,
}

/// 改配置。
///
/// **不是「把整份新配置发过来」**，是「基于哪一版、改哪几个字段」。
/// 整份发过来的话，两个人同时改就必然有一个人的改动被悄悄吃掉 ——
/// 而那正是 §1 里 cc-switch 那批 issue 的形状。
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
    Replace { path: String, value: PatchValue },
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

/// 整份文本写回去（文本模式）。
///
/// **和 `PATCH` 是两条路，但同一扇门。**表单模式改字段，文本模式改整份
/// —— 后者是前者的退路（§3.8：结构性的增删一律引导到文本模式），而两者
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

/// 一段时间的汇总（§4.3、§8）。
///
/// **实测和估算分开，没有价格的单独数。**「今日 $12.40 实测 + ~$0.80
/// 估算，另有 3 条没有价格」比一个混在一起的 $13.20 诚实得多 —— 后者
/// 看起来是个确定的数字。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Summary {
    pub requests: i64,
    pub failed: i64,
    /// 本地应答的次数。**是个正向数字**，单独显示（§4.8）
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
    /// 走订阅型上游的请求数。**不参与金额合计**（§4.3.1）——
    /// 订阅制的边际成本是零，按价目表算出来的数字是纯虚构的
    #[serde(default)]
    pub subscription_requests: i64,
    /// 那些请求用掉的 token。**它才是订阅用户该看的量**
    #[serde(default)]
    pub subscription_tokens: i64,
    /// 缓存命中一共省下了多少微分（§4.4）。
    ///
    /// **算的是差额，不是「缓存读花了多少」** —— 用户想知道的是「如果
    /// 没命中要多花多少」
    #[serde(default)]
    pub cache_saved_micros: i64,
    /// 价目表的快照日期。**成本旁边要标它**（§4.3.0）—— 一个两个月前
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
    /// 这个成本是估的吗。**界面上要标出来**（§4.3）
    pub cost_estimated: bool,
    pub error: Option<String>,
    /// 本地应答的（§4.8）
    pub local: bool,
    /// 服务它的那家怎么收钱：`per-token` / `subscription` / `unknown`
    #[serde(default)]
    pub billing: String,
    /// 缓存命中省下了多少微分。`None` = 算不出来（§4.4）
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
    /// **已脱敏**。这段文字会被复制到 issue 里（§9.7）
    pub text: String,
    /// 原本多长。**截断了要能说出来** —— 不说的话用户会以为请求本身
    /// 就长这样
    pub original_len: usize,
    pub truncated: bool,
}

/// 一个上游的订阅额度（§4.3.2）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderQuota {
    pub provider: String,
    pub windows: Vec<QuotaWindow>,
}

/// L3 测速要花多少（§4.6）。
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
    /// 用户会以为那就是全部代价（§4.6）
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
    /// 不指定模型的测速结果没有意义（§4.6）
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

/// 「过去 7 天，有 3 个请求把你的 API key 发给了 relay-cn」（§5.0）。
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

/// 观测这一层现在能不能写（§8）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageStatus {
    /// 「正常」/「磁盘快满了…」/「磁盘几乎满了…」
    pub level: String,
    /// 记了多少条
    pub rows: i64,
    /// 请求体占了多少字节
    pub blob_bytes: u64,
    /// **转发受影响了吗。永远是 false** —— 观测挂了，代理照跑（§4.7）
    pub forwarding_affected: bool,
}

/// 首次运行时写下第一个上游。
///
/// **只在还没有 provider 时可用**。之后改配置走 §3.8 的双向同步（M2），
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

/// 一次任务（§7.9）。
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
    /// 不同的结论（§4.3）
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
    /// **没有价格就是 None，不是 0**（§4.3）
    pub cost_micros: Option<i64>,
    pub duration_ms: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDetail {
    pub session: SessionView,
    pub turns: Vec<TurnView>,
}

// ---------------------------------------------------------- 上游行为基线

/// 一个上游最近是不是变了（§5.2 防线三）。
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

/// 一处发现（§5.3）。
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
/// **不存任何东西**（§7.12）：这是此刻磁盘上的真实情况，页面关了就没了。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResponse {
    pub findings: Vec<ScanFinding>,
    pub mcp: Vec<McpView>,
    pub skills: Vec<SkillView>,
    pub hooks: Vec<HookView>,
    /// 同名但配置不同的 MCP server 名字（§7.12 矩阵上要标记号）
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

/// 在矩阵上点一下（§7.12）。
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
/// **每个字段都对应规则里能写的一个条件**（§3.4）。默认值就是一个最
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
    /// 的省钱手段直接扔掉，而且不会察觉**（§3.4）
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
    /// 客户端自己发的辅助请求（§4.8）。空 = 真实的用户请求
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
/// 「为什么没走我以为的那条」和「走了哪条」是同一个问题的两面（§3.4）。
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
    /// 这条路会不会伤到 prompt cache。**要直说 —— 它决定账单**（§3.4）
    pub hurts_cache: bool,
    /// 候选链里此刻熔断着的那些。**试算是静态的，但熔断是当下的事实**
    pub circuit_open: Vec<String>,
}

// ---------------------------------------------------------- 客户端接管
//
// **注意 `DetectedClient` 和上面的 `ClientView` 是两个东西**：那个是
// config.yaml 里的一把网关密钥，这个是本机上装着的一个 AI 客户端 App。
// 中文都叫「客户端」，混起来的话，「有几个客户端」这句话就有两个答案。

/// 一个客户端此刻的样子（§7.11）。
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
    /// 弹「是不是没生效」是狼来了（§7.11）
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
    /// 用哪把网关密钥。不写就用第一把 —— §0.6：为「一个 key 就够」的人设计
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
