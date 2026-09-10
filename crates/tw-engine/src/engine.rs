//! 路由引擎：规则 + 策略组。
//!
//! **层 0 不是特例**（DESIGN.md §3.4）。「不配规则也能跑」在内部被展开成
//! 「一个包含全部 provider 的 fallback 组 + 一条指向它的兜底规则」，
//! 所以引擎只有一条代码路径，用户看到的却是零配置可用。

use serde::{Deserialize, Serialize};

use crate::facts::RequestFacts;
use crate::rule::{MatchError, When};

/// 策略组类型。
///
/// **默认是 `fallback` 而不是负载均衡**，理由很硬：负载均衡会摧毁
/// prompt cache。缓存按 provider 隔离，长会话每轮跳一家就永远命中不了，
/// 而缓存命中与否成本差 5 到 10 倍 —— 随机分流的代价可能是账单翻几倍，
/// 换来的是「分散负载」，**而单用户桌面场景根本没有需要分散的负载**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum GroupType {
    /// 按顺序，第一个健康的就用。缓存友好
    #[default]
    Fallback,
    /// 手动指定一个。缓存友好
    Select,
    /// 轮流。**必须开会话粘滞，否则缓存全废**
    LoadBalance,
    /// 选最快的。判据是**真实流量测出来的 TTFB**，样本不够时用启动时
    /// 那次零成本的 L1 握手计时补（§4.6）。
    ///
    /// **测不到的那些排最后，而不是排最前。**「没测到」不等于「慢」，
    /// 但把它排前面就等于放弃了「选最快的」这个承诺；排最后它仍然是
    /// 故障转移的备选。
    UrlTest,
    /// 选最便宜的。**按输入单价排，输出单价只做同价时的次序**。
    ///
    /// 编码 agent 的输入输出比通常在 10:1 以上（长上下文、短 diff），
    /// 所以输入单价主导。**算不出价钱的排最后** —— 「最便宜」是一句
    /// 关于钱的承诺，而挑一个不知道多少钱的，完全可能是最贵的那个。
    Cheapest,
}

impl GroupType {
    /// 这个策略会不会让 prompt cache 不稳定。
    ///
    /// UI 上选中时要给一句提示 —— **不要让用户为了省 20% 的单价，
    /// 付出丢掉 90% 缓存折扣的代价**（§3.4）。
    /// 这个**策略类型**天生会不会让 prompt cache 不稳定。
    ///
    /// 具体到一个组还要看它的配置 —— 用 [`Group::hurts_cache`]。
    pub fn hurts_cache(&self) -> bool {
        matches!(self, GroupType::LoadBalance | GroupType::UrlTest)
    }

    /// 排顺序时要不要用到运行时的数字。
    ///
    /// **`fallback` 和 `select` 不需要**，而它们是绝大多数人的配置
    /// （§0.6）—— 于是那条路上一个 HashMap 都不用建。
    pub fn needs_runtime(&self) -> bool {
        !matches!(self, GroupType::Fallback | GroupType::Select)
    }

    pub fn label(&self) -> &'static str {
        match self {
            GroupType::Fallback => "按顺序",
            GroupType::Select => "手动选",
            GroupType::LoadBalance => "轮流",
            GroupType::UrlTest => "选最快",
            GroupType::Cheapest => "选最便宜",
        }
    }
}

/// 排顺序时才知道的那些数字。**引擎是纯函数，这些从外面传进来** ——
/// 于是数据面和试算页（§7.11）走的是同一段逻辑，试算不会「算出一个
/// 和真实转发不一样的结果」。
#[derive(Debug, Clone, Default)]
pub struct Facts {
    /// 会话指纹（§7.9）。`None` = 认不出来这是哪次会话
    pub session: Option<String>,
    /// 轮转的种子。认不出会话时用它 —— 通常是请求序号
    pub seq: u64,
    /// 每家的典型 TTFB（毫秒）。**缺席 = 样本不够**，不是「很快」
    pub ttfb_ms: std::collections::HashMap<String, u32>,
    /// 每家跑这个模型的单价，(输入, 输出)，微分/百万 token。
    /// **缺席 = 算不出价钱**，不是「免费」
    pub price: std::collections::HashMap<String, (i64, i64)>,
}

/// 一组上游，以及从里面挑一个的策略。
///
/// **`deny_unknown_fields`** —— 这个字段在 Rust 里叫 `kind`，在 YAML 里
/// 叫 `type`，所以「顺手写成 `kind:`」几乎是必然会发生的。没有这一行的
/// 时候它会被静默丢掉，用户得到一个 fallback 组，然后困惑于「我明明配了
/// 负载均衡」。这个项目已经栽过一次同样的（`listen: { addr: ... }`）。
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub name: String,
    #[serde(default, rename = "type")]
    pub kind: GroupType,
    pub providers: Vec<String>,
    /// 同一个会话固定走同一家，缓存才能保住。
    ///
    /// **默认开，而且这个默认值是这一节最重要的一行。**不开的话，一次
    /// 长会话每轮跳一家，prompt cache 全部失效 —— 而缓存命中与否成本
    /// 差 5 到 10 倍（§3.4）。「分散负载」换来的是账单翻几倍，而单用户
    /// 桌面场景根本没有需要分散的负载。
    ///
    /// 真想要纯轮询的人写一句 `session_affinity: false`，那是个明确的
    /// 选择；而默认关掉，是让每一个不知道这件事的人默默付那笔钱。
    #[serde(default = "default_true")]
    pub session_affinity: bool,
    /// `select` 用：当前选中的那个
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
}

impl Group {
    /// 这个组**按现在这份配置**会不会让 prompt cache 不稳定。
    ///
    /// 三种情况分开看：
    ///
    /// - `load-balance` **开了粘滞就不伤缓存**：同一次对话始终落在同
    ///   一家，缓存该命中还是命中。把它一律标成危险是个假警报，而假
    ///   警报的代价是用户学会忽略这一栏的所有提示（§0.6：没有风险的
    ///   时候要说「安全」）。
    /// - `url-test` **会伤**：排序随实测延迟变，一次对话中途完全可能
    ///   换家。
    /// - `cheapest` **不伤**：单价在一次对话里不会变，所以顺序是稳的
    ///   —— 它换家的时机和 `fallback` 一样，只在上游不健康时。
    pub fn hurts_cache(&self) -> bool {
        match self.kind {
            GroupType::LoadBalance => !self.session_affinity,
            GroupType::UrlTest => true,
            GroupType::Fallback | GroupType::Select | GroupType::Cheapest => false,
        }
    }
}

/// 按策略排序。**纯函数** —— 同样的输入永远给同样的顺序，试算页因此
/// 能如实预告数据面会怎么走。
pub fn order_by(g: &Group, members: &[String], f: &Facts) -> Vec<String> {
    match g.kind {
        // 这两种的顺序在 `expand_group` 里就定好了
        GroupType::Fallback | GroupType::Select => members.to_vec(),
        GroupType::LoadBalance => rotate(g, members, f),
        GroupType::UrlTest => {
            // 有样本的按 TTFB 升序；没样本的保持原有相对次序排在后面。
            // **`sort_by_key` 是稳定排序**，所以同速的两家不会每次换位
            // —— 那会让 prompt cache 白白多断一次
            let mut out = members.to_vec();
            out.sort_by_key(|p| f.ttfb_ms.get(p).copied().unwrap_or(u32::MAX));
            out
        }
        GroupType::Cheapest => {
            let mut out = members.to_vec();
            out.sort_by_key(|p| match f.price.get(p) {
                // 输入单价主导，输出单价只做同价时的次序
                Some((i, o)) => (*i, *o),
                // 算不出价钱的排最后
                None => (i64::MAX, i64::MAX),
            });
            out
        }
    }
}

/// `load-balance` 的轮转。
///
/// **认得出会话就固定一家**（§3.5：不开粘滞的话，长会话每轮跳一家，
/// prompt cache 全废，而缓存命中与否成本差 5 到 10 倍 —— 「分散负载」
/// 换来的可能是账单翻几倍）。
///
/// 认不出会话时按 `seq` 轮转。**选中的那一家排头，其余顺次跟上**，
/// 一个都不少 —— 故障转移还要用它们。
fn rotate(g: &Group, members: &[String], f: &Facts) -> Vec<String> {
    if members.is_empty() {
        return Vec::new();
    }
    let start = if g.session_affinity {
        match &f.session {
            // **同一个会话永远落在同一家。**用会话指纹取模，不用计数器
            // —— 计数器会让「重启之后同一个会话换了一家」，而那正是
            // 粘滞要防的
            Some(s) => (hash64(s) % members.len() as u64) as usize,
            // **认不出会话就钉住第一家，不要轮转。**
            //
            // 用户开粘滞是在说「别在一次对话里换家」。认不出这条属于
            // 哪次对话的时候去轮转，做的正好是他要求的反面 —— 而那
            // 时候的代价照样是缓存全废。钉住会让这部分流量集中在一家，
            // 但**不均衡是可以看见的，缓存失效不是**。
            None => 0,
        }
    } else {
        (f.seq % members.len() as u64) as usize
    };
    let mut out = Vec::with_capacity(members.len());
    out.extend_from_slice(&members[start..]);
    out.extend_from_slice(&members[..start]);
    out
}

/// 会话指纹 → 一个数。FNV-1a。
///
/// **不用 `DefaultHasher`。**它是 std 的 SipHash，文档明说不保证跨版本
/// 稳定 —— 而这个数字决定一次对话钉在哪一家。它一变，所有正在进行的
/// 会话会在同一时刻集体换家，**一次性把全部 prompt cache 作废**，而
/// 现场表现只是「今天账单突然高了」，没有任何东西指向一次 Rust 升级。
///
/// 自己写五行，换来的是这个映射永远不变。不用于安全，只要稳定。
fn hash64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 横切的安全策略（DESIGN.md §5.1、§5.2）。
///
/// **只能收紧，不能放松。**这是三层配置里最外面那一层，而它的合并规则
/// 是并集 —— 一条 `guard` 规则不小心写少了，不会把 provider 上配好的
/// 保护削掉。让横切策略安全的正是这一条：**加一条规则永远不会让系统
/// 变得更不安全**，所以人敢往里加规则。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Guard {
    /// 额外要脱的类别。和 provider 上配的取并集
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redact: Vec<tw_redact::rules::Kind>,
    /// 这条路径上一律当成不受信任的上游看待（§5.2）。
    ///
    /// **只能从「信任」收到「不信任」**，反过来写不生效
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub untrusted: bool,
}

impl Guard {
    /// 并集。**只加不减** —— 这就是「只能收紧」那句话的全部实现。
    pub fn merge(&mut self, other: &Guard) {
        for k in &other.redact {
            if !self.redact.contains(k) {
                self.redact.push(*k);
            }
        }
        self.untrusted |= other.untrusted;
    }
    pub fn is_empty(&self) -> bool {
        self.redact.is_empty() && !self.untrusted
    }
}

/// 改写请求参数。
///
/// **这是和流量代理最本质的分歧**（§3.4）：Clash 的规则只能决定走哪个
/// proxy，因为它能做的就是转发字节。我们在协议层，能改的东西多得多 ——
/// 而 `set` 让「降级」成为可能，而不是只能「拒绝」。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetAction {
    /// 换一个模型。
    ///
    /// **注意这会作废整个 prompt cache** —— 和 §3.9 说的「改个对外名字」
    /// 不是一回事，那个不影响缓存因为发给上游的名字没变；这里是真的换了
    /// 一个模型。而「预算超了」恰恰最容易发生在缓存已经暖好的长会话里，
    /// 在那个时刻降级**很可能比不降级还贵**（§3.4）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    /// 只在新会话开始时应用，跑到一半不动它。
    ///
    /// **长会话场景下这应该是默认值**（§3.4），但会话识别是 M3 的事 ——
    /// 现在这个字段只被记录和展示，不生效。写在这里是为了让配置格式
    /// 不必二次改动。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub only_at_session_start: bool,
}

impl SetAction {
    /// 后面的覆盖前面的同名字段（§3.4 的累积规则）。
    pub fn merge(&mut self, other: &SetAction) {
        if other.model.is_some() {
            self.model = other.model.clone();
        }
        if other.max_tokens.is_some() {
            self.max_tokens = other.max_tokens;
        }
        if other.thinking.is_some() {
            self.thinking = other.thinking;
        }
        self.only_at_session_start |= other.only_at_session_start;
    }

    pub fn is_empty(&self) -> bool {
        self.model.is_none() && self.max_tokens.is_none() && self.thinking.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// **每条规则有名字**。日志里、UI 里、试算结果里都能引用它 ——
    /// 「命中第 4 条」远不如「命中『带缓存的必须走官方』」有用。
    pub name: String,
    #[serde(default)]
    pub when: When,
    /// **可以直接指 provider，不需要先建组**（§3.4 层 1）。大多数分流
    /// 需求到这一层就解决了，不必引入策略组这个概念。
    ///
    /// 阶段二的规则（含 `provider_would_be`）**不允许写它** —— 那会成环。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// 改写请求参数。**从所有命中的规则累积**，不只是第一条。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<SetAction>,
    /// 直接拒绝，带一句给客户端看的原因。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<String>,
    /// 横切的安全策略（§5.1）。**从所有命中的规则累积，而且只能收紧。**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<Guard>,
}

/// 一次路由的结果，**带上为什么**。
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// 选中的 provider，按优先级排 —— 第一个是首选，后面是故障转移的
    /// 备选。**故障转移和路由是同一次决策的两个部分**，分开算会让「切
    /// 到哪一家」变成一个没人能解释的结果。
    pub candidates: Vec<String>,
    /// 命中了哪条规则。日志和 UI 都要显示它
    pub matched_rule: String,
    /// 经过了哪个组（直接指 provider 时是 None）
    pub via_group: Option<String>,
    /// 累积起来的参数改写
    pub set: SetAction,
    /// 累积起来的安全策略。**并集，只加不减**
    pub guard: Guard,
}

/// 阶段一结束时可能是「不让干」。
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Route(Decision),
    /// `deny` 命中。**带一句给客户端看的原因** —— 一个没有理由的拒绝
    /// 和一个 bug 在用户眼里没有区别。
    Deny {
        rule: String,
        reason: String,
    },
}

/// 阶段二的结果。
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome2 {
    /// 继续，带上累积后的参数改写和安全策略
    Proceed(SetAction, Guard),
    Deny {
        rule: String,
        reason: String,
    },
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum RouteError {
    #[error("没有任何规则命中，而且没有兜底规则。加一条不带 `when` 的规则收尾。")]
    NoMatch,
    #[error(
        "规则 `{0}` 用了 `provider_would_be`，同时又写了 `to`。这会成环 —— 那个条件的值要等路由决定完才知道，而 `to` 正是路由决定的东西。这类规则只能写 `set` / `deny`。"
    )]
    PhaseTwoWithTo(String),
    #[error("规则 `{0}` 既没有 `to` 也没有 `deny`，也不改任何参数 —— 它命中了也什么都不做。")]
    NoAction(String),
    #[error("规则 `{rule}` 指向 `{target}`，但既没有这个 provider 也没有这个组")]
    UnknownTarget { rule: String, target: String },
    #[error("组 `{0}` 里一个 provider 都没有")]
    EmptyGroup(String),
    #[error(transparent)]
    Match(#[from] MatchError),
}

pub struct Engine {
    routes: Vec<Route>,
    groups: Vec<Group>,
    providers: Vec<String>,
}

impl Engine {
    /// 建引擎。
    ///
    /// **层 0 在这里被展开**：没有 routes 时补一条兜底规则指向一个自动
    /// 生成的、包含全部 provider 的 fallback 组。这样下面的求值逻辑
    /// 只有一条路径。
    pub fn new(providers: Vec<String>, mut groups: Vec<Group>, mut routes: Vec<Route>) -> Self {
        // **没有 provider 时什么都不合成。**合成一个空组会让它过不了
        // `validate`（空组是配置错误），而零 provider 的配置必须合法 ——
        // 否则首次运行时 core 起不来，控制面也起不来，UI 连「你还没配
        // 上游」都说不出口（tw-config::validate 里那段注释）。
        //
        // 这条今天已经立过一次，又被这里破坏了一次。测试抓住了。
        if routes.is_empty() && !providers.is_empty() {
            const ALL: &str = "__all__";
            groups.push(Group {
                name: ALL.to_string(),
                kind: GroupType::Fallback,
                providers: providers.clone(),
                session_affinity: false,
                selected: None,
            });
            routes.push(Route {
                name: "默认：按声明顺序故障转移".to_string(),
                when: When::default(),
                to: Some(ALL.to_string()),
                set: None,
                deny: None,
                guard: None,
            });
        }
        Self {
            routes,
            groups,
            providers,
        }
    }

    /// 加载时校验。**规则写错了要在这里说，不要等请求进来** ——
    /// 一条指向不存在的 provider 的规则，在运行时的表现是每个命中它的
    /// 请求都失败，而用户看不出是哪条规则的问题。
    pub fn validate(&self) -> Result<(), RouteError> {
        for r in &self.routes {
            r.when.validate()?;
            // **这条禁令是必需的**：允许阶段二的规则写 `to`，求值就直接
            // 成环了（§3.4）。在校验阶段挡下来，而不是在运行时。
            if r.when.is_phase_two() && r.to.is_some() {
                return Err(RouteError::PhaseTwoWithTo(r.name.clone()));
            }
            if r.to.is_none() && r.deny.is_none() && r.set.as_ref().is_none_or(|s| s.is_empty()) {
                return Err(RouteError::NoAction(r.name.clone()));
            }
            if r.to.is_some() {
                self.resolve_target(r)?;
            }
        }
        for g in &self.groups {
            if g.providers.is_empty() {
                return Err(RouteError::EmptyGroup(g.name.clone()));
            }
        }
        Ok(())
    }

    /// 阶段一：按请求性质选出候选 provider。
    ///
    /// **自上而下，首个命中决定 `to` 和 `deny`；`set` 从所有命中的规则
    /// 累积**（§3.4）。两者规则不同是有理由的：去向只能有一个，而参数
    /// 改写是可以叠加的横切策略。
    pub fn route(&self, facts: &RequestFacts) -> Result<Outcome, RouteError> {
        let mut set = SetAction::default();
        let mut guard = Guard::default();
        let mut chosen: Option<&Route> = None;

        for r in &self.routes {
            // 阶段二的规则在这一轮完全跳过 —— 它们的条件还没法求值。
            if r.when.is_phase_two() || !r.when.matches(facts)? {
                continue;
            }
            if let Some(s) = &r.set {
                set.merge(s);
            }
            if let Some(g) = &r.guard {
                guard.merge(g);
            }
            if chosen.is_none() && (r.to.is_some() || r.deny.is_some()) {
                chosen = Some(r);
            }
        }

        let Some(r) = chosen else {
            return Err(RouteError::NoMatch);
        };
        if let Some(reason) = &r.deny {
            return Ok(Outcome::Deny {
                rule: r.name.clone(),
                reason: reason.clone(),
            });
        }
        let (candidates, via_group) = self.resolve_target(r)?;
        Ok(Outcome::Route(Decision {
            candidates,
            matched_rule: r.name.clone(),
            via_group,
            set,
            guard,
        }))
    }

    /// 阶段二：知道了具体走哪家之后，再跑一遍含 `provider_would_be` 的规则。
    ///
    /// **故障转移换了 provider 之后必须重跑这一步**（§3.4）。否则「走中转
    /// 的一律脱敏」这条规则，在从官方转移到中转时会漏掉 —— 而那正是最
    /// 需要它的时刻。
    pub fn phase_two(
        &self,
        facts: &RequestFacts,
        provider: &str,
        base: &SetAction,
        base_guard: &Guard,
    ) -> Result<Outcome2, RouteError> {
        let mut set = base.clone();
        let mut guard = base_guard.clone();
        for r in &self.routes {
            if !r.when.is_phase_two() || !r.when.matches_with_provider(facts, provider)? {
                continue;
            }
            if let Some(reason) = &r.deny {
                return Ok(Outcome2::Deny {
                    rule: r.name.clone(),
                    reason: reason.clone(),
                });
            }
            if let Some(s) = &r.set {
                set.merge(s);
            }
            // **「走中转的一律脱敏」这条规则活在这里。**它的条件要等
            // 选完上游才知道，而故障转移从官方切到中转的那一刻，正是
            // 最需要它的时刻
            if let Some(g) = &r.guard {
                guard.merge(g);
            }
        }
        Ok(Outcome2::Proceed(set, guard))
    }

    fn resolve_target(&self, r: &Route) -> Result<(Vec<String>, Option<String>), RouteError> {
        let Some(to) = &r.to else {
            return Ok((Vec::new(), None));
        };
        // provider 优先于组。同名时按 provider 解释 —— 而校验会挡住
        // 同名的情况，所以这个优先级实际上不会被用到。
        if self.providers.iter().any(|p| p == to) {
            return Ok((vec![to.clone()], None));
        }
        if let Some(g) = self.groups.iter().find(|g| g.name == *to) {
            return Ok((self.expand_group(g), Some(g.name.clone())));
        }
        Err(RouteError::UnknownTarget {
            rule: r.name.clone(),
            target: to.clone(),
        })
    }

    fn expand_group(&self, g: &Group) -> Vec<String> {
        match g.kind {
            // 顺序就是优先级。第一个健康的就用，缓存持续命中。
            GroupType::Fallback => g.providers.clone(),
            GroupType::Select => {
                // 选中的排头，其余仍然留着做故障转移 —— **手动选一家不
                // 等于放弃容错**，那家挂了照样该切。
                let mut out = Vec::with_capacity(g.providers.len());
                if let Some(sel) = &g.selected
                    && g.providers.contains(sel)
                {
                    out.push(sel.clone());
                }
                out.extend(
                    g.providers
                        .iter()
                        .filter(|p| Some(*p) != g.selected.as_ref())
                        .cloned(),
                );
                out
            }
            // 这三种要运行时的数字才排得出来。**这里只给集合，
            // 顺序由 `order` 定** —— 它是纯函数，数据面和试算页都调它。
            GroupType::LoadBalance | GroupType::UrlTest | GroupType::Cheapest => {
                g.providers.clone()
            }
        }
    }

    /// 把候选按这个组的策略排好。
    ///
    /// **`expand_group` 给的是集合，这里给的是顺序。**分成两步是因为
    /// 顺序要用运行时的数字，而路由决策本身必须是纯的、可试算的。
    ///
    /// 排完之后**所有候选都还在**，只是次序变了 —— 故障转移要用到它们
    /// （§4.2）。「选最快的」不等于「只用最快的那一家」。
    pub fn order(&self, group: Option<&str>, members: &[String], f: &Facts) -> Vec<String> {
        let Some(g) = group.and_then(|n| self.groups.iter().find(|g| g.name == n)) else {
            return members.to_vec();
        };
        order_by(g, members, f)
    }

    pub fn groups(&self) -> &[Group] {
        &self.groups
    }
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(model: &str) -> RequestFacts {
        RequestFacts {
            model: model.into(),
            client: "claude-code".into(),
            dialect: "anthropic".into(),
            ..Default::default()
        }
    }

    fn route(name: &str, when_yaml: &str, to: &str) -> Route {
        Route {
            name: name.into(),
            when: serde_yaml_ng::from_str(when_yaml).unwrap(),
            to: Some(to.into()),
            set: None,
            deny: None,
            guard: None,
        }
    }

    /// 只关心去向的测试用它，省掉每处都 match 一遍。
    fn candidates(e: &Engine, f: &RequestFacts) -> Vec<String> {
        match e.route(f).unwrap() {
            Outcome::Route(d) => d.candidates,
            other => panic!("该路由，实际 {other:?}"),
        }
    }

    fn decision(e: &Engine, f: &RequestFacts) -> Decision {
        match e.route(f).unwrap() {
            Outcome::Route(d) => d,
            other => panic!("该路由，实际 {other:?}"),
        }
    }

    #[test]
    fn layer_zero_needs_no_rules_at_all() {
        // 「只配 provider」是最小可用配置，而且对不少人就够了：
        // 「官方为主，挂了走中转」零规则就能满足（§3.4 层 0）。
        let e = Engine::new(vec!["official".into(), "relay".into()], vec![], vec![]);
        let d = decision(&e, &facts("claude-sonnet-4-5"));
        assert_eq!(d.candidates, ["official", "relay"], "按声明顺序故障转移");
        assert!(e.validate().is_ok());
    }

    #[test]
    fn no_providers_yields_no_synthesised_group() {
        // 零 provider 是首次运行的合法状态。合成一个空组会让配置校验
        // 失败，而那会让 core 起不来 —— 于是 UI 连「你还没配上游」都
        // 说不出口。
        let e = Engine::new(vec![], vec![], vec![]);
        assert!(e.groups().is_empty());
        assert!(e.routes().is_empty());
        assert!(e.validate().is_ok(), "零 provider 的配置必须合法");
    }

    #[test]
    fn layer_zero_is_not_a_special_case_in_the_code() {
        // 它被展开成「一个全量 fallback 组 + 一条兜底规则」，所以求值
        // 只有一条路径。这个测试盯着那个展开，而不是它的表面行为。
        let e = Engine::new(vec!["a".into()], vec![], vec![]);
        assert_eq!(e.groups().len(), 1);
        assert_eq!(e.routes().len(), 1);
        assert!(e.routes()[0].when.is_catch_all());
    }

    #[test]
    fn a_rule_can_point_straight_at_a_provider_without_a_group() {
        // 大多数分流需求到这一层就解决了，不必引入策略组这个概念。
        let e = Engine::new(
            vec!["official".into(), "relay".into()],
            vec![],
            vec![
                route("opus 走官方", "{ model: claude-opus-* }", "official"),
                route("兜底", "{}", "relay"),
            ],
        );
        let d = decision(&e, &facts("claude-opus-4-5"));
        assert_eq!(d.candidates, ["official"]);
        assert_eq!(d.via_group, None);
        assert_eq!(d.matched_rule, "opus 走官方");
    }

    #[test]
    fn rules_are_tried_in_order_and_the_first_match_wins() {
        let e = Engine::new(
            vec!["a".into(), "b".into()],
            vec![],
            vec![
                route("先", "{}", "a"),
                route("后", "{ model: claude-* }", "b"),
            ],
        );
        assert_eq!(candidates(&e, &facts("claude-opus-4-5")), ["a"]);
    }

    #[test]
    fn no_catch_all_and_no_match_says_what_to_add() {
        // 「没有规则命中」是配置问题，而错误信息要说清下一步。
        let e = Engine::new(
            vec!["a".into()],
            vec![],
            vec![route("只管 opus", "{ model: claude-opus-* }", "a")],
        );
        let err = e.route(&facts("claude-sonnet-4-5")).unwrap_err();
        assert_eq!(err, RouteError::NoMatch);
        assert!(err.to_string().contains("兜底"));
    }

    #[test]
    fn a_group_expands_to_its_providers_in_order() {
        let g = Group {
            name: "pool".into(),
            kind: GroupType::Fallback,
            providers: vec!["official".into(), "relay".into()],
            session_affinity: false,
            selected: None,
        };
        let e = Engine::new(
            vec!["official".into(), "relay".into()],
            vec![g],
            vec![route("走池子", "{}", "pool")],
        );
        let d = decision(&e, &facts("x"));
        assert_eq!(d.candidates, ["official", "relay"]);
        assert_eq!(d.via_group.as_deref(), Some("pool"));
    }

    #[test]
    fn select_puts_the_chosen_one_first_but_keeps_the_rest_for_failover() {
        // **手动选一家不等于放弃容错。**那家挂了照样该切，否则「select」
        // 就变成了「单点故障」。
        let g = Group {
            name: "pool".into(),
            kind: GroupType::Select,
            providers: vec!["a".into(), "b".into(), "c".into()],
            session_affinity: false,
            selected: Some("b".into()),
        };
        let e = Engine::new(
            vec!["a".into(), "b".into(), "c".into()],
            vec![g],
            vec![route("x", "{}", "pool")],
        );
        assert_eq!(candidates(&e, &facts("x")), ["b", "a", "c"]);
    }

    #[test]
    fn a_rule_pointing_nowhere_is_caught_at_load_time() {
        // 运行时的表现是「每个命中它的请求都失败」，而用户看不出是哪条
        // 规则的问题。
        let e = Engine::new(vec!["a".into()], vec![], vec![route("x", "{}", "typo")]);
        let err = e.validate().unwrap_err();
        assert!(matches!(err, RouteError::UnknownTarget { .. }));
        assert!(err.to_string().contains("typo"));
    }

    #[test]
    fn a_broken_comparison_in_a_rule_fails_validation() {
        let e = Engine::new(
            vec!["a".into()],
            vec![],
            vec![route("x", r#"{ input_tokens: "200k" }"#, "a")],
        );
        assert!(e.validate().is_err());
    }

    #[test]
    fn an_empty_group_is_caught() {
        let g = Group {
            name: "empty".into(),
            kind: GroupType::Fallback,
            providers: vec![],
            session_affinity: false,
            selected: None,
        };
        let e = Engine::new(vec!["a".into()], vec![g], vec![route("x", "{}", "empty")]);
        assert!(matches!(e.validate(), Err(RouteError::EmptyGroup(_))));
    }

    fn route_full(
        name: &str,
        when_yaml: &str,
        to: Option<&str>,
        set: Option<SetAction>,
        deny: Option<&str>,
    ) -> Route {
        Route {
            name: name.into(),
            when: serde_yaml_ng::from_str(when_yaml).unwrap(),
            to: to.map(String::from),
            set,
            deny: deny.map(String::from),
            guard: None,
        }
    }

    #[test]
    fn set_accumulates_from_every_matching_rule_while_to_takes_the_first() {
        // 两者规则不同是有理由的：**去向只能有一个，而参数改写是可以
        // 叠加的横切策略**（§3.4）。
        let e = Engine::new(
            vec!["a".into(), "b".into()],
            vec![],
            vec![
                route_full(
                    "关掉思考",
                    "{}",
                    None,
                    Some(SetAction {
                        thinking: Some(false),
                        ..Default::default()
                    }),
                    None,
                ),
                route_full("走 a", "{}", Some("a"), None, None),
                route_full(
                    "再改模型",
                    "{}",
                    None,
                    Some(SetAction {
                        model: Some("cheap".into()),
                        ..Default::default()
                    }),
                    None,
                ),
                route_full("走 b", "{}", Some("b"), None, None),
            ],
        );
        let d = decision(&e, &facts("x"));
        assert_eq!(d.candidates, ["a"], "去向是第一条有 to 的");
        assert_eq!(d.set.thinking, Some(false));
        assert_eq!(
            d.set.model.as_deref(),
            Some("cheap"),
            "后面的规则也累积进来了"
        );
    }

    #[test]
    fn a_later_set_overrides_an_earlier_one_on_the_same_field() {
        let e = Engine::new(
            vec!["a".into()],
            vec![],
            vec![
                route_full(
                    "先",
                    "{}",
                    Some("a"),
                    Some(SetAction {
                        model: Some("first".into()),
                        ..Default::default()
                    }),
                    None,
                ),
                route_full(
                    "后",
                    "{}",
                    None,
                    Some(SetAction {
                        model: Some("second".into()),
                        ..Default::default()
                    }),
                    None,
                ),
            ],
        );
        assert_eq!(
            decision(&e, &facts("x")).set.model.as_deref(),
            Some("second")
        );
    }

    #[test]
    fn deny_carries_a_reason_the_client_can_read() {
        // 一个没有理由的拒绝，和一个 bug，在用户眼里没有区别。
        let e = Engine::new(
            vec!["a".into()],
            vec![],
            vec![
                route_full(
                    "脚本不许用 opus",
                    "{ model: claude-opus-* }",
                    None,
                    None,
                    Some("脚本不允许调用 Opus"),
                ),
                route_full("兜底", "{}", Some("a"), None, None),
            ],
        );
        match e.route(&facts("claude-opus-4-5")).unwrap() {
            Outcome::Deny { rule, reason } => {
                assert_eq!(rule, "脚本不许用 opus");
                assert_eq!(reason, "脚本不允许调用 Opus");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(candidates(&e, &facts("claude-sonnet-4-5")), ["a"]);
    }

    #[test]
    fn phase_two_rules_are_invisible_to_phase_one() {
        // 它们的条件还没法求值 —— provider_would_be 要等路由跑完。
        let e = Engine::new(
            vec!["official".into(), "relay".into()],
            vec![],
            vec![
                route_full(
                    "走中转的降级",
                    "{ provider_would_be: relay }",
                    None,
                    Some(SetAction {
                        thinking: Some(false),
                        ..Default::default()
                    }),
                    None,
                ),
                route_full("兜底", "{}", Some("relay"), None, None),
            ],
        );
        let d = decision(&e, &facts("x"));
        assert_eq!(d.matched_rule, "兜底");
        assert_eq!(d.set.thinking, None, "阶段一不该看见阶段二的 set");
    }

    #[test]
    fn phase_two_applies_once_the_provider_is_known() {
        let e = Engine::new(
            vec!["official".into(), "relay".into()],
            vec![],
            vec![
                route_full(
                    "走中转的降级",
                    "{ provider_would_be: relay }",
                    None,
                    Some(SetAction {
                        thinking: Some(false),
                        ..Default::default()
                    }),
                    None,
                ),
                route_full("兜底", "{}", Some("relay"), None, None),
            ],
        );
        let base = SetAction::default();
        match e
            .phase_two(&facts("x"), "relay", &base, &Guard::default())
            .unwrap()
        {
            Outcome2::Proceed(s, _) => assert_eq!(s.thinking, Some(false)),
            other => panic!("{other:?}"),
        }
        // 换一家就不该命中了 —— 这正是故障转移后必须重跑的理由
        match e
            .phase_two(&facts("x"), "official", &base, &Guard::default())
            .unwrap()
        {
            Outcome2::Proceed(s, _) => assert_eq!(s.thinking, None),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn phase_two_can_match_a_list_of_providers() {
        // `provider_would_be: [a, b]` 就是 §3.4 里 `any_of` 的实际形态，
        // 不必发明一个关键字。
        let e = Engine::new(
            vec!["a".into(), "b".into(), "c".into()],
            vec![],
            vec![
                route_full(
                    "这两家都不可靠",
                    "{ provider_would_be: [a, b] }",
                    None,
                    Some(SetAction {
                        thinking: Some(false),
                        ..Default::default()
                    }),
                    None,
                ),
                route_full("兜底", "{}", Some("a"), None, None),
            ],
        );
        let base = SetAction::default();
        for (p, want) in [("a", Some(false)), ("b", Some(false)), ("c", None)] {
            match e
                .phase_two(&facts("x"), p, &base, &Guard::default())
                .unwrap()
            {
                Outcome2::Proceed(s, _) => assert_eq!(s.thinking, want, "provider={p}"),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn a_phase_two_rule_with_a_target_is_refused_at_load_time() {
        // **允许的话就直接成环了**：provider_would_be 的值要等路由决定完
        // 才知道，而 `to` 正是路由决定的东西。
        let e = Engine::new(
            vec!["a".into()],
            vec![],
            vec![route_full(
                "成环",
                "{ provider_would_be: a }",
                Some("a"),
                None,
                None,
            )],
        );
        let err = e.validate().unwrap_err();
        assert!(matches!(err, RouteError::PhaseTwoWithTo(_)));
        assert!(err.to_string().contains("成环"), "{err}");
    }

    #[test]
    fn a_rule_that_does_nothing_at_all_is_refused() {
        // 命中了也什么都不做的规则，多半是写漏了 —— 而它在运行时完全
        // 静默，用户只会觉得「我明明配了」。
        let e = Engine::new(
            vec!["a".into()],
            vec![],
            vec![route_full("空的", "{ model: x }", None, None, None)],
        );
        assert!(matches!(e.validate(), Err(RouteError::NoAction(_))));
    }

    #[test]
    fn only_load_balance_is_flagged_as_bad_for_cache() {
        // 这张表要在 UI 上直接显示，因为它决定了用户的账单（§3.4）。
        assert!(!GroupType::Fallback.hurts_cache());
        assert!(!GroupType::Select.hurts_cache());
        assert!(GroupType::LoadBalance.hurts_cache());
        // 而默认必须是不伤缓存的那个
        assert_eq!(GroupType::default(), GroupType::Fallback);
    }

    #[test]
    fn the_decision_says_which_rule_matched() {
        // 「命中第 4 条」远不如「命中『带缓存的必须走官方』」有用。
        let e = Engine::new(
            vec!["official".into()],
            vec![],
            vec![route("带缓存的必须走官方", "{ cache: true }", "official")],
        );
        let mut f = facts("x");
        f.cache = true;
        assert_eq!(decision(&e, &f).matched_rule, "带缓存的必须走官方");
    }

    #[test]
    fn writing_kind_instead_of_type_is_an_error_not_a_silent_downgrade() {
        // 字段在 Rust 里叫 `kind`，在 YAML 里叫 `type` —— 顺手写成
        // `kind:` 几乎必然发生。静默丢掉的话，用户得到的是一个 fallback
        // 组，然后困惑于「我明明配了负载均衡」。
        let e = serde_yaml_ng::from_str::<Group>("name: g\nkind: load-balance\nproviders: [a]\n")
            .unwrap_err();
        assert!(e.to_string().contains("kind"), "{e}");
        // 写对的那个照常认得
        let g: Group =
            serde_yaml_ng::from_str("name: g\ntype: load-balance\nproviders: [a]\n").unwrap();
        assert_eq!(g.kind, GroupType::LoadBalance);
    }

    #[test]
    fn a_typo_in_a_route_field_is_an_error_too() {
        let e = serde_yaml_ng::from_str::<Route>("name: r\nton: 官方\n").unwrap_err();
        assert!(e.to_string().contains("ton"), "{e}");
    }

    fn grp(kind: GroupType, sticky: bool) -> Group {
        Group {
            name: "池子".into(),
            kind,
            providers: vec!["甲".into(), "乙".into(), "丙".into()],
            session_affinity: sticky,
            selected: None,
        }
    }

    #[test]
    fn load_balance_actually_rotates_instead_of_always_picking_the_first() {
        // **这条是回归测试。**曾经 `load-balance` 和 `fallback` 行为完全
        // 一样：引擎给出集合，而没有任何一层去转它，于是 6 个请求 6 次
        // 落在第一家 —— 一个宣称做完了、实际什么都没做的功能。
        let g = grp(GroupType::LoadBalance, false);
        let firsts: Vec<String> = (0..6)
            .map(|seq| {
                let f = Facts {
                    seq,
                    ..Default::default()
                };
                order_by(&g, &g.providers, &f)[0].clone()
            })
            .collect();
        assert_eq!(firsts, vec!["甲", "乙", "丙", "甲", "乙", "丙"]);
    }

    #[test]
    fn rotating_keeps_every_candidate_because_failover_still_needs_them() {
        // 「轮到乙」不等于「甲和丙不要了」——那一家挂了还要能切
        let g = grp(GroupType::LoadBalance, false);
        let f = Facts {
            seq: 1,
            ..Default::default()
        };
        assert_eq!(order_by(&g, &g.providers, &f), vec!["乙", "丙", "甲"]);
    }

    #[test]
    fn session_affinity_pins_one_conversation_to_one_upstream() {
        // **不粘的话，长会话每轮跳一家，prompt cache 全废**，而缓存
        // 命中与否成本差 5 到 10 倍（§3.5）
        let g = grp(GroupType::LoadBalance, true);
        let mut seen = std::collections::HashSet::new();
        for seq in 0..20 {
            let f = Facts {
                session: Some("会话-abc".into()),
                seq,
                ..Default::default()
            };
            seen.insert(order_by(&g, &g.providers, &f)[0].clone());
        }
        assert_eq!(seen.len(), 1, "同一个会话跳家了：{seen:?}");
    }

    #[test]
    fn different_sessions_do_land_on_different_upstreams() {
        // 粘滞不能粘成「所有会话都挤在一家」——那就不是均衡了
        let g = grp(GroupType::LoadBalance, true);
        let seen: std::collections::HashSet<String> = (0..60)
            .map(|i| {
                let f = Facts {
                    session: Some(format!("会话-{i}")),
                    ..Default::default()
                };
                order_by(&g, &g.providers, &f)[0].clone()
            })
            .collect();
        assert!(seen.len() > 1, "所有会话都挤在同一家：{seen:?}");
    }

    #[test]
    fn url_test_puts_the_fastest_first_and_the_unmeasured_last() {
        // **「没测到」不等于「慢」，但排前面就等于放弃了「选最快的」**
        let g = grp(GroupType::UrlTest, false);
        let mut ttfb = std::collections::HashMap::new();
        ttfb.insert("丙".to_string(), 120u32);
        ttfb.insert("甲".to_string(), 400u32);
        // 乙没有样本
        let f = Facts {
            ttfb_ms: ttfb,
            ..Default::default()
        };
        assert_eq!(order_by(&g, &g.providers, &f), vec!["丙", "甲", "乙"]);
    }

    #[test]
    fn two_equally_fast_upstreams_do_not_swap_places_every_request() {
        // 同速时换来换去会让 prompt cache 白白多断一次 —— 稳定排序
        let g = grp(GroupType::UrlTest, false);
        let ttfb: std::collections::HashMap<String, u32> = [
            ("甲".to_string(), 100u32),
            ("乙".to_string(), 100),
            ("丙".to_string(), 100),
        ]
        .into_iter()
        .collect();
        let f = Facts {
            ttfb_ms: ttfb,
            ..Default::default()
        };
        for _ in 0..5 {
            assert_eq!(order_by(&g, &g.providers, &f), vec!["甲", "乙", "丙"]);
        }
    }

    #[test]
    fn cheapest_sorts_by_input_price_and_puts_the_unpriced_last() {
        // **「最便宜」是一句关于钱的承诺** —— 挑一个不知道多少钱的，
        // 完全可能是最贵的那个
        let g = grp(GroupType::Cheapest, false);
        let price: std::collections::HashMap<String, (i64, i64)> = [
            ("甲".to_string(), (3_000_000i64, 15_000_000i64)),
            ("丙".to_string(), (1_800_000, 9_000_000)),
        ]
        .into_iter()
        .collect();
        let f = Facts {
            price,
            ..Default::default()
        };
        assert_eq!(order_by(&g, &g.providers, &f), vec!["丙", "甲", "乙"]);
    }

    #[test]
    fn cheapest_uses_the_output_price_only_to_break_a_tie() {
        let g = Group {
            providers: vec!["甲".into(), "乙".into()],
            ..grp(GroupType::Cheapest, false)
        };
        let price: std::collections::HashMap<String, (i64, i64)> = [
            ("甲".to_string(), (1_000i64, 9_000i64)),
            ("乙".to_string(), (1_000, 5_000)),
        ]
        .into_iter()
        .collect();
        let f = Facts {
            price,
            ..Default::default()
        };
        assert_eq!(order_by(&g, &g.providers, &f), vec!["乙", "甲"]);
    }

    #[test]
    fn fallback_and_select_never_get_reordered() {
        for kind in [GroupType::Fallback, GroupType::Select] {
            let g = grp(kind, false);
            let f = Facts {
                seq: 7,
                ttfb_ms: [("丙".to_string(), 1u32)].into_iter().collect(),
                ..Default::default()
            };
            assert_eq!(
                order_by(&g, &g.providers, &f),
                g.providers,
                "{kind:?} 被重排了"
            );
            assert!(!kind.needs_runtime());
        }
    }
    #[test]
    fn the_session_to_slot_mapping_is_frozen_forever() {
        // **这条测试就是那个「不许改」的锁。**这个数字决定一次对话钉在
        // 哪一家；改掉它等于让所有正在进行的会话同时换家，一次性作废
        // 全部 prompt cache —— 而现场表现只是「今天账单突然高了」。
        //
        // 真要换算法的话，先想清楚怎么让已经在跑的会话平滑过去。
        assert_eq!(hash64(""), 0xcbf2_9ce4_8422_2325);
        // 这三个值是用一份独立的 FNV-1a 实现（python）算出来的，
        // 不是把这段代码的输出抄回来 —— 后者只能证明它没变，证明不了
        // 它是对的
        assert_eq!(hash64("会话-abc"), 16_378_437_173_232_644_658);
        assert_eq!(hash64("a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn load_balance_is_sticky_unless_you_explicitly_turn_it_off() {
        // **默认值站在缓存这边。**不写 session_affinity 的人，是不知道
        // 这件事的人 —— 而默认关掉就是让他默默付那笔钱
        let y = "name: 池子\ntype: load-balance\nproviders: [甲, 乙]\n";
        let g: Group = serde_yaml_ng::from_str(y).unwrap();
        assert!(g.session_affinity, "默认没开粘滞");
        assert!(!g.hurts_cache(), "开着粘滞还报「会伤缓存」是个假警报");

        let off: Group = serde_yaml_ng::from_str(&format!("{y}session_affinity: false\n")).unwrap();
        assert!(!off.session_affinity);
        assert!(off.hurts_cache(), "关了粘滞就该直说会伤缓存");
    }

    #[test]
    fn an_unrecognisable_session_pins_instead_of_rotating() {
        // 用户开粘滞是在说「别在一次对话里换家」。认不出这条属于哪次
        // 对话时去轮转，做的正好是他要求的反面
        let g = grp(GroupType::LoadBalance, true);
        for seq in 0..6 {
            let f = Facts {
                session: None,
                seq,
                ..Default::default()
            };
            assert_eq!(
                order_by(&g, &g.providers, &f)[0],
                "甲",
                "认不出会话时还在转"
            );
        }
    }

    #[test]
    fn cheapest_does_not_get_flagged_as_cache_hostile() {
        // 单价在一次对话里不会变，所以它的顺序是稳的 —— 换家的时机和
        // fallback 一样，只在上游不健康时。**没有风险的时候要说「安全」**
        let mut g = grp(GroupType::Cheapest, false);
        g.kind = GroupType::Cheapest;
        assert!(!g.hurts_cache());
        let u = Group {
            kind: GroupType::UrlTest,
            ..g.clone()
        };
        assert!(u.hurts_cache(), "url-test 的排序随实测延迟变，会伤");
    }
}
