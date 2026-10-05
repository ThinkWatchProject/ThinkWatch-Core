//! WebSocket 升级代理（第三个集成细节）。
//!
//! sub2api 给 Codex CLI 提供 `/backend-api/codex/responses` 的 WS 桥接
//! （客户端 WS ↔ 上游 HTTP/SSE）。要覆盖这条链路，数据面得能代理一次
//! 升级，而不只是 HTTP + SSE。
//!
//! # 这一层最容易犯的错：把它做成一根管子
//!
//! 直接把两边的帧对着倒，是最省事的写法，也是**一条绕过整条管线的
//! 合法后门** —— 出站脱敏、工具墙全都不会发生，而
//! 用户完全看不出这条路和别的路有什么不同。重放那次已经踩过一模一样
//! 的坑：**任何绕过主管线的路径都要把管线上的保护重新点一遍。**
//!
//! 所以这里每一帧文本都过同一套：
//!
//! - 客户端 → 上游：内容过滤和出站脱敏，和普通请求同一套函数、同一份全局规则 ——
//!   观察档记录，处置档拒绝、删除或替换；
//! - 上游 → 客户端：先把占位符换回去，再喂给工具调用审查。
//!
//! 每个 `response.create` 发出去的模型名**和 HTTP 那条路的一跳同一套**（见 [`Naming`]）：
//! 规则给这一家指定的模型原样发，阶段二改的名字原样发，别的 —— 这一帧写的、阶段一改写成的、
//! 插件改成的 —— 按别名表对到这一家自己的名称。发出去之前过密钥的模型范围（`allow`），规则的
//! 参数改写（`set` 的最大输出、关思考）照 HTTP 那条路改。这一家服务不了要的别名、密钥不让用
//! 要发的模型、规则拒绝了这一帧，这一帧不发，替它回一个 `response.failed`，连接照常。回答里
//! 的模型名写回客户端用的那个（[`crate::answer_model`]）。
//!
//! Realtime 的连接（`/v1/realtime`）模型写在升级请求的查询串里：升级时按它路由、过密钥的
//! 模型范围、对别名（[`Naming::connect`]），发给上游的查询串写这一家自己的名称。
//!
//! # 一轮一个请求
//!
//! **Responses 的连接上每个 `response.create` 是一个请求**（见 [`turn`]）：从这一帧到这一次
//! 回答完，开始、路由、结局三条事件，存储层记一行，带着这一次回答的用量，照 HTTP 那条路查价；
//! 密钥的用量上限、并发上限、这一家的位置（`max_concurrent`）都按轮算，闲着的连接什么都不占。
//! 连接本身不留行，连不上上游的除外。Realtime 和别的路径的连接照旧**整条连接一行**：它们的
//! 回答不按 Responses 的事件收尾，分不出一轮一轮。Realtime 的每一次回答在 `response.done` 里
//! 报用量，这一行带着它们加起来的数（[`realtime_usage`]），断开时照 HTTP 那条路查价、算进密钥
//! 的用量；别的路径的连接不知道用量的写法，不带。
//!
//! 脚本插件也在这条路上跑（见 [`crate::plugin`]）：客户端发来的每个
//! `response.create` 是一次请求。**这条路只有一跳**（升级时就连定了那一家，不换），
//! 所以每个 `response.create` 过一遍请求钩子：上游是这条连接连的那一家，模型名是发给它的
//! 那个（上面那一套定的）。位置和 HTTP 那条路的一跳一样 —— 内容过滤先查
//! 客户端的原话（删过的话插件拿到的是删过的那一帧），插件改过的再查一遍、只报插件加进来
//! 的，然后才脱敏、发出。上游每一次回答（`response.created` 到 `response.completed`）
//! 起一组回答钩子的实例，排在占位符还原之后、工具墙之前。插件出错而策略是拒绝时，切掉的
//! 是那一次回答，连接照常。
//!
//! **插件只管 Responses 的 WebSocket**（每个 `response.create` 是一次对话请求）。别的路径
//! 上的连接（比如 Realtime 的 `/v1/realtime`）不属于插件处理的任何一种请求：所有插件都
//! 不管，原样接上，什么都不记（见 `server::upgrade`）。
//!
//! # 两条明说的边界
//!
//! **一、走代理的上游不代理 WS。**代理是给 reqwest 配的，而这里
//! 是自己建连。悄悄绕过用户配的代理，等于把他以为在代理后面的流量直接
//! 发出去 —— 那比不支持严重得多，所以宁可明确拒绝。
//!
//! **二、二进制帧原样转发。**脱敏规则是文本规则，对二进制没有意义；
//! 而假装检查过它，比说清楚没检查更糟。

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::protocol::Message as UpMsg;

use crate::error::GatewayError;
use crate::state::AppState;
use tw_types::{Msg, msg};

pub(crate) mod turn;

/// `Option<WebSocketUpgrade>` 的替身。
///
/// axum 0.8 只给 `WebSocketUpgrade` 实现了 `FromRequestParts`，没有
/// `OptionalFromRequestParts` —— 而**这个 handler 是全局的 fallback**，
/// 绝大多数请求根本不是升级请求。提取失败在这里不是错误，是常态。
pub struct MaybeUpgrade(pub Option<axum::extract::WebSocketUpgrade>);

impl<S> axum::extract::FromRequestParts<S> for MaybeUpgrade
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        if !is_upgrade(&parts.headers) {
            return Ok(MaybeUpgrade(None));
        }
        Ok(MaybeUpgrade(
            axum::extract::WebSocketUpgrade::from_request_parts(parts, state)
                .await
                .ok(),
        ))
    }
}

/// 这是一次 WebSocket 升级请求吗。
pub fn is_upgrade(headers: &axum::http::HeaderMap) -> bool {
    let has = |k: &str, v: &str| {
        headers
            .get(k)
            .and_then(|x| x.to_str().ok())
            .is_some_and(|s| s.to_ascii_lowercase().contains(v))
    };
    has("upgrade", "websocket") && has("connection", "upgrade")
}

/// 把 `http(s)://host/path` 换成 `ws(s)://host/path`。
///
/// **地址先按 HTTP 那条路拼**（[`crate::forward::upstream_url`]），再换协议头 ——
/// 查询串里的 `key=` 是网关密钥，HTTP 那条路会剔掉它；这里另写一份拼法的话，
/// 就会漏掉这一步，把密钥发给上游。
pub fn upstream_url(base: &str, path: &str, query: Option<&str>) -> String {
    let http = crate::forward::upstream_url(base, path, query);
    if let Some(rest) = http.strip_prefix("https://") {
        format!("wss://{rest}")
    } else {
        format!("ws://{}", http.trim_start_matches("http://"))
    }
}

/// Realtime 的连接：`/v1/realtime`（不带 `/v1` 的也认）。它的模型写在查询串里
/// （`?model=gpt-realtime`），不在帧里。
pub fn realtime(path: &str) -> bool {
    let p = path.trim_end_matches('/');
    p.strip_prefix("/v1").unwrap_or(p) == "/realtime"
}

/// 查询串里的 `model`，百分号编码解开。没有这一项（或者是空的）是 `None`。
pub fn query_model(query: Option<&str>) -> Option<String> {
    query?
        .split('&')
        .find_map(|pair| pair.strip_prefix("model="))
        .map(percent_decode)
        .filter(|m| !m.is_empty())
}

/// 查询串里的 `model` 换成 `model`。**别的项一个字节都不动**：重新编码整个查询串会改掉
/// 上游认的写法。
pub fn with_query_model(query: &str, model: &str) -> String {
    query
        .split('&')
        .map(|pair| {
            if pair.starts_with("model=") {
                format!("model={}", percent_encode(model))
            } else {
                pair.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// 查询串一项的值：`%XX` 解成字节，`+` 是空格（表单编码）。不是 UTF-8 的照替换字符算
fn percent_decode(s: &str) -> String {
    let hex = |c: u8| (c as char).to_digit(16);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let pair = (b[i] == b'%' && i + 2 < b.len())
            .then(|| hex(b[i + 1]).zip(hex(b[i + 2])))
            .flatten();
        match (pair, b[i]) {
            (Some((h, l)), _) => {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
            (None, b'+') => out.push(b' '),
            (None, c) => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 写进查询串的值：字母数字和 `-._~:` 原样，别的按字节编码
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 要连的那一家，以及路由是怎么选中它的。
pub struct Upstream {
    /// `ws(s)://` 开头的完整地址（见 [`upstream_url`]）
    pub url: String,
    /// 连上游时带的头：凭据，和配置里给这一家写的那些
    pub headers: Vec<(String, String)>,
    pub provider: tw_config::Provider,
    /// 走的路由、命中的规则和经过的策略组。**路由事件在这边发**，和 HTTP 那条路
    /// 报的是同一个形状，只是要等握手有了结果
    pub route: String,
    pub rule: String,
    pub group: Option<String>,
    /// 附加了参数改写的规则（见 `tw_engine::Decision::rewritten_by`）。路由事件里报
    pub rewritten_by: Vec<String>,
    /// 升级时就定下的、发给这一家的模型名，和客户端写的不一样时才有（尝试链上记它）：
    /// Realtime 的连接查询串里的模型对过的名字。Responses 的连接看 [`Naming::fixed`]
    pub model: Option<String>,
}

/// 各项防护此刻的档位和规则。**升级那一刻取一次**：一条连接活多久，就按
/// 它开始时的配置走多久，和普通请求按开始时的运行时走是同一个道理。
pub struct Rules {
    pub redact_mode: tw_config::SecurityMode,
    pub redact: Arc<tw_guard::redact::rules::RuleSet>,
    pub inspect_mode: tw_config::SecurityMode,
    pub tools: Arc<tw_guard::tools::rules::Rules>,
    /// 内容过滤
    pub screen: crate::guard::Screen,
}

/// 这条连接上的插件：**升级那一刻取的那一份表**，一条连接活多久就用它多久。
pub struct Plugins {
    pub pool: Arc<crate::plugin::pool::Pool>,
    pub set: Arc<crate::plugin::PluginSet>,
    /// 客户端是哪个应用（范围和 `ctx.client` 看它）
    pub client: Option<String>,
}

/// 这条连接上每个 `response.create` 发给上游的模型名和参数怎么定：**和 HTTP 那条路的一跳
/// 同一套**（见 [`crate::sent`]）。模型名依次：
///
/// - 规则给这一家指定了模型：原样发指定的那个；
/// - 阶段二的规则改了名字：原样发（写的时候已经知道是哪一家）；
/// - 别的是客户端那一侧的名称 —— 这一帧写的，或者阶段一改写成的 —— 按别名表对到这一家自己的
///   名称（[`crate::models::resolve`]）。插件再改名的，改出来的名字一样对。
///
/// 要发的名字先过密钥的模型范围（`allow`，[`Self::allows`]），和 HTTP 那条路的准入、每一跳
/// 同一个判据。
///
/// **去向在升级时就定了**：升级请求没有正文，按模型路由的规则对它不适用（见
/// `server::upgrade`），连哪一家、指定的模型都在那次的决定里，一条连接用到底。**规则的其余
/// 部分每一帧按这一帧求**：到那时才知道这一帧要的模型、多长、带不带工具，和 HTTP 那条路每个
/// 请求求一遍是同一个道理 —— 阶段一的参数改写（`set`，从所有命中的规则累积）和拒绝，阶段二。
///
/// 发不出去时**这一帧不发**，替它回一个 `response.failed`，连接照常：这一家服务不了要的别名
/// （不把别名本身发给一个不认识它的上游）、密钥不让用要发的模型、规则拒绝了它。不是别名的
/// 名字照旧原样发，这一家有没有它由它自己回答。
///
/// Responses 的连接每一帧用它（[`Self::frame`]）。Realtime 的连接没有 `response.create` 这种
/// 请求，模型写在升级请求的查询串里：升级时用一次（[`Self::connect`]）。
pub struct Naming {
    /// 升级那一刻的配置和路由引擎：一条连接活多久就用它多久，和 [`Rules`] 一样
    pub config: Arc<tw_config::Config>,
    pub engine: Arc<tw_engine::Engine>,
    /// 升级时路由的决定
    pub decision: tw_engine::Decision,
    /// 这条连接连的那一家
    pub provider: tw_config::Provider,
    /// 网关密钥的名字：规则按它认客户端，模型范围是它的
    pub client: String,
    /// 升级的路径。一帧按这条路径上的一次请求读（[`crate::client_api::read`]）
    pub path: String,
}

/// 一帧 `response.create` 发出去的样子（[`Naming::frame`]）。
struct Outgoing {
    /// 发给这一家的模型名。这一帧没写模型名时是空的：什么都不改
    model: String,
    /// 规则的参数改写：阶段一（按这一帧求）和阶段二累积的。模型名不看这里，看 `model`
    set: tw_engine::SetAction,
    /// 附加了参数改写的规则：阶段一的，加上阶段二的。这一轮的开始和路由事件里报
    rewritten_by: Vec<String>,
}

/// 一帧为什么没发出去（[`Naming::frame`]）。
struct NotSent {
    why: GatewayError,
    /// 规则拒绝了它：**这一帧照样留一行**，和 HTTP 那条路被规则拒绝的请求一样（见
    /// [`turn::denied`]）。别的 —— 密钥不让用要发的模型、这一家服务不了要的别名、规则求不了
    /// 值 —— 不留，和 HTTP 那条路准入没过一样
    denied: Option<Denied>,
}

impl NotSent {
    fn plain(why: GatewayError) -> Box<Self> {
        Box::new(Self { why, denied: None })
    }
}

/// 拒绝了一帧的那条规则，在哪个阶段。
pub(crate) enum Denied {
    /// 阶段一：它就是决定这一帧去向的那条
    PhaseOne(String),
    /// 阶段二：记在路由事件的 `denied_by` 上
    PhaseTwo(String),
}

/// 要发的名字为什么发不出去（[`Naming::name`]）。
enum Unsent {
    /// 密钥不让用
    Barred(GatewayError),
    /// 这一家服务不了要的别名
    Unserved(GatewayError),
}

impl Unsent {
    fn into_error(self) -> GatewayError {
        match self {
            Unsent::Barred(e) | Unsent::Unserved(e) => e,
        }
    }
}

/// 模型写在升级请求里的连接（Realtime）连不连这一家（[`Naming::connect`]）。
pub enum Connect {
    /// 连：发给它的模型名，和阶段二附加了参数改写的规则
    To {
        model: String,
        rewritten_by: Vec<String>,
    },
    /// 不连这一家，看下一家：它服务不了要的别名，或者要发给它的名字密钥不让用（`barred`）。
    /// 和 HTTP 那条路挑候选时跳过它们一样
    Skip { barred: bool, why: GatewayError },
    /// 整个拒绝，不换下一家：阶段二的规则拒绝了这次升级（`rule` 是哪条），或者规则求不了值。
    /// 和 HTTP 那条路一跳上的阶段二一样
    Refused {
        rule: Option<String>,
        why: GatewayError,
    },
}

impl Naming {
    /// 客户端那一侧的名称发给这一家时叫什么（[`crate::models::resolve`]）。`None` = 这一家
    /// 服务不了这个别名
    fn resolve(&self, catalog: &tw_engine::Catalog, name: &str) -> Option<String> {
        crate::models::resolve(&self.config, catalog, &self.provider, name)
    }

    /// 每一帧都发的那个名字，升级时就定了的话：规则给这一家指定了模型，或者阶段一把模型
    /// 改写了（按别名表对到这一家）。**尝试链上记它**，和 HTTP 那条路每一跳记发出去的名字
    /// 一样；要看每一帧写的是什么的（别名、阶段二、插件改名）升级时说不上来，是 None
    fn fixed(&self, catalog: &tw_engine::Catalog) -> Option<String> {
        let facts = tw_engine::RequestFacts {
            client: self.client.clone(),
            ..Default::default()
        };
        let asked = self
            .engine
            .asked_of(&facts, &self.decision, &self.provider.name, None);
        match asked.origin {
            tw_engine::Origin::Pinned => Some(asked.model),
            tw_engine::Origin::Rule => self.resolve(catalog, &asked.model),
            _ => None,
        }
    }

    /// 这把密钥的模型范围（`allow`）。不写是 `None`：什么都放行
    fn allow(&self) -> Option<&[String]> {
        self.config
            .clients
            .iter()
            .find(|c| c.name == self.client)
            .and_then(|c| c.allow.as_deref())
    }

    /// 密钥的 `allow` 放不放行要发的名字 `model`。**和 HTTP 那条路同一个判据**（准入、每一跳、
    /// 插件改名都看它）：
    ///
    /// - 客户端那一侧的名称（客户端写的、阶段一改写的、插件改的，可能是别名）按目录的规矩看：
    ///   写上游的模型名也放行列了它的别名（[`tw_engine::Catalog::allows`]）；
    /// - 原样发出的名字（`as_written`：指定模型、阶段二改的）按名字本身对 glob：它不经过
    ///   别名表，也就没有别名可继承。
    ///
    /// 和清单问没问到无关：`allow` 写在配置里。
    fn allows(&self, catalog: &tw_engine::Catalog, model: &str, as_written: bool) -> bool {
        self.allow().is_none_or(|patterns| {
            if as_written {
                patterns
                    .iter()
                    .any(|p| tw_engine::rule::glob_match(p, model))
            } else {
                catalog.allows(model, patterns)
            }
        })
    }

    /// 这一家要的名字（`asked`，见 [`tw_engine::Engine::asked_of`]）发出去叫什么：先过密钥的
    /// `allow`，再按别名表对到这一家（指定的、阶段二改的原样）。`decision` 是这一次的决定，
    /// `client` 是客户端写的模型名。要的是空的（没写模型名）什么都不改、不判断
    fn name(
        &self,
        catalog: &tw_engine::Catalog,
        decision: &tw_engine::Decision,
        client: &str,
        asked: &tw_engine::Asked,
    ) -> Result<String, Unsent> {
        if asked.model.is_empty() {
            return Ok(String::new());
        }
        let written = asked.origin.as_written();
        if !self.allows(catalog, &asked.model, written) {
            return Err(Unsent::Barred(self.barred(decision, client, asked)));
        }
        if written {
            return Ok(asked.model.clone());
        }
        self.resolve(catalog, &asked.model)
            .ok_or_else(|| Unsent::Unserved(self.unserved(client, &asked.model)))
    }

    /// 一帧按这条路径上的一次请求读出的样子：规则的条件按它求值，开始事件里的模型名、输入的
    /// 估算也从它来
    fn read(&self, frame: &serde_json::Value) -> crate::client_api::Reading {
        let mut reading = crate::client_api::read(&self.path, None, Some(frame));
        reading.facts.client = self.client.clone();
        reading
    }

    /// 一帧 `response.create`（读出的性质是 `facts`，见 [`Self::read`]）发给这一家的样子。这一帧
    /// 发不出去时是告诉客户端的那个错误：规则拒绝了它、规则求不了值、密钥不让用要发的模型，
    /// 或者这一家服务不了要的别名。
    fn frame(
        &self,
        catalog: &tw_engine::Catalog,
        facts: &tw_engine::RequestFacts,
    ) -> Result<Outgoing, Box<NotSent>> {
        let decision = self.this_frame(facts)?;
        let p = &self.provider;
        let (mut set, renamed, two) = match self.engine.phase_two(facts, &p.name, &decision.set) {
            Ok(tw_engine::Outcome2::Proceed {
                set,
                model,
                rewritten_by,
            }) => (set, model, rewritten_by),
            Ok(tw_engine::Outcome2::Deny { rule, reason }) => {
                tracing::info!(%rule, provider = %p.name, "a phase-two rule denied a WebSocket request");
                return Err(Box::new(NotSent {
                    why: denied(rule.clone(), reason),
                    denied: Some(Denied::PhaseTwo(rule)),
                }));
            }
            Err(e) => return Err(NotSent::plain(rule_failed(e))),
        };
        let asked = self
            .engine
            .asked_of(facts, &decision, &p.name, renamed.as_deref());
        let model = self
            .name(catalog, &decision, &facts.model, &asked)
            .map_err(|u| NotSent::plain(u.into_error()))?;
        // Codex 后端不认最大输出：HTTP 那条路发给它之前也会删掉（`crate::chatgpt::shape_passthrough`）
        if p.effective_protocol() == Some(tw_config::Protocol::Chatgpt) {
            set.max_tokens = None;
        }
        let mut rewritten_by = decision.rewritten_by;
        for r in two {
            if !rewritten_by.contains(&r) {
                rewritten_by.push(r);
            }
        }
        Ok(Outgoing {
            model,
            set,
            rewritten_by,
        })
    }

    /// 这一帧的决定。**去向是升级时的**（候选、经过的组、指定的模型），参数改写按这一帧重新
    /// 求：阶段一从所有命中的规则累积 `set`，条件按这一帧的性质看，和 HTTP 那条路一样。
    ///
    /// 按这一帧求出来是拒绝的，这一帧不发：一条按模型拒绝的规则在 HTTP 那条路上拦得住，在这条
    /// 路上也要拦得住 —— 升级时还不知道模型，那时拦不到它。
    fn this_frame(
        &self,
        facts: &tw_engine::RequestFacts,
    ) -> Result<tw_engine::Decision, Box<NotSent>> {
        match self.engine.route(facts) {
            Ok(tw_engine::Outcome::Route(d)) => Ok(tw_engine::Decision {
                set: d.set,
                rewritten_by: d.rewritten_by,
                ..self.decision.clone()
            }),
            Ok(tw_engine::Outcome::Deny { rule, reason }) => {
                tracing::info!(%rule, "a rule denied a WebSocket request");
                Err(Box::new(NotSent {
                    why: denied(rule.clone(), reason),
                    denied: Some(Denied::PhaseOne(rule)),
                }))
            }
            Err(e) => Err(NotSent::plain(GatewayError::config(msg!(
                "gw.route.failed", detail = e => "Routing failed: {detail}"
            )))),
        }
    }

    /// 模型写在升级请求里的连接（Realtime 查询串里的 `model`，就是 `facts.model`）连不连这一家、
    /// 发给它的模型名。和一帧 `response.create` 同一套（[`Self::name`]），按升级时的决定跑一遍。
    ///
    /// 阶段二拒绝了、而这一家本来连得上的，才算拒绝：和 HTTP 那条路一样，服务不了的候选在尝试
    /// 之前就跳过了，轮不到它的阶段二。
    pub fn connect(
        &self,
        catalog: &tw_engine::Catalog,
        facts: &tw_engine::RequestFacts,
    ) -> Connect {
        let p = &self.provider;
        let (renamed, rewritten_by, refused) =
            match self.engine.phase_two(facts, &p.name, &self.decision.set) {
                Ok(tw_engine::Outcome2::Proceed {
                    model,
                    rewritten_by,
                    ..
                }) => (model, rewritten_by, None),
                Ok(tw_engine::Outcome2::Deny { rule, reason }) => (
                    None,
                    Vec::new(),
                    Some((Some(rule.clone()), denied(rule, reason))),
                ),
                Err(e) => (None, Vec::new(), Some((None, rule_failed(e)))),
            };
        let asked = self
            .engine
            .asked_of(facts, &self.decision, &p.name, renamed.as_deref());
        match (
            self.name(catalog, &self.decision, &facts.model, &asked),
            refused,
        ) {
            (Err(Unsent::Barred(why)), _) => Connect::Skip { barred: true, why },
            (Err(Unsent::Unserved(why)), _) => Connect::Skip { barred: false, why },
            (Ok(_), Some((rule, why))) => Connect::Refused { rule, why },
            (Ok(model), None) => Connect::To {
                model,
                rewritten_by,
            },
        }
    }

    /// 密钥不让用要发的名字：和 HTTP 那条路的准入同几句话，说出名字是哪来的。`client` 是
    /// 客户端写的
    fn barred(
        &self,
        decision: &tw_engine::Decision,
        client: &str,
        asked: &tw_engine::Asked,
    ) -> GatewayError {
        let key = self.client.clone();
        let why = match asked.origin {
            tw_engine::Origin::Client => msg!(
                "gw.model.not_allowed", key = key, model = client =>
                "Gateway key `{key}` may not use model {model}. GET /v1/models lists the \
                 models that are available."
            ),
            tw_engine::Origin::Rule | tw_engine::Origin::PhaseTwo => msg!(
                "gw.model.not_allowed_rewritten", from = client, model = asked.model.clone(),
                key = key =>
                "A routing rule rewrote model {from} to {model}, which gateway key `{key}` may not \
                 use. GET /v1/models lists the models that are available."
            ),
            tw_engine::Origin::Pinned => msg!(
                "gw.model.pinned_not_allowed", rule = decision.matched_rule.clone(),
                model = asked.model.clone(), upstream = self.provider.name.clone(), key = key =>
                "Rule `{rule}` pins model {model} on `{upstream}`, which gateway key `{key}` may \
                 not use."
            ),
        };
        GatewayError::request(why)
    }

    /// 这一家服务不了别名 `alias`。`client` 是这一帧写的：和别名不一样就是阶段一的规则改写的
    fn unserved(&self, client: &str, alias: &str) -> GatewayError {
        let models = self
            .config
            .aliases
            .find(alias)
            .map(|a| a.models.join(", "))
            .unwrap_or_default();
        let upstream = self.provider.name.clone();
        let why = if client == alias {
            msg!(
                "gw.ws.alias_unserved", model = alias, upstream = upstream, models = models =>
                "This WebSocket connection goes to upstream `{upstream}`, which offers none of the \
                 models of alias {model} ({models}), so the request was not sent."
            )
        } else {
            msg!(
                "gw.ws.alias_unserved_rewritten",
                from = client, model = alias, upstream = upstream, models = models =>
                "A routing rule rewrote model {from} to alias {model}. This WebSocket connection \
                 goes to upstream `{upstream}`, which offers none of its models ({models}), so the \
                 request was not sent."
            )
        };
        GatewayError::request(why)
    }
}

/// 规则拒绝了这个请求：和 HTTP 那条路同一句
fn denied(rule: String, reason: String) -> GatewayError {
    GatewayError::denied(msg!(
        "gw.route.denied", rule = rule, reason = reason =>
        "Rule `{rule}` denied this request: {reason}"
    ))
}

/// 规则求不了值：和 HTTP 那条路同一句
fn rule_failed(e: tw_engine::RouteError) -> GatewayError {
    GatewayError::config(msg!(
        "gw.route.rule_failed", detail = e => "A rule could not be evaluated: {detail}"
    ))
}

/// 一次连接里两个方向各自的状态。
struct Pipes {
    /// **整条连接一本账。**每帧各起一本的话，第二帧的
    /// `<<TW_SECRET_1>>` 会和第一帧的撞车（见 `tw_guard::redact::replace::apply` 的注释）
    ledger: tw_guard::redact::replace::Ledger,
    /// 工具调用审查关着的时候没有它
    wall: Option<tw_guard::tools::wall::Wall>,
    /// 正在跑的那次回答的 id（`response.created` 里的）。切掉这次回答时的
    /// `response.failed` 要说是哪一次
    response: Option<String>,
    /// 这次回答被切掉了（回答钩子出错而策略是拒绝）、已经替它发过 `response.failed`：
    /// 它剩下的帧（包括上游自己的收尾）一帧都不再发，下一次回答照常
    dropping: bool,
    rules: Rules,
    provider: String,
    /// 这条连接的号。整条连接一行的（Realtime 和别的路径）是那一行的；Responses 的连接自己
    /// 不留行，不在哪一轮里的帧（上游的、客户端的）报出去的事件挂在它上面（见 [`Self::event_id`]）
    id: u64,
    /// Responses 的连接上在跑的几轮（见 [`turn`]）。别的连接没有
    turns: Option<turn::Turns>,
    /// 整条连接一行的 Realtime 连接：每一次回答的用量（`response.done`）加到这一行上
    realtime: bool,
    /// 范围里可能有插件时才有
    plugins: Option<Plugins>,
    /// 每个 `response.create` 发出去的模型名怎么定。Responses 的连接才有
    naming: Option<Naming>,
    /// 最近一次 `response.create`：客户端要的模型、发出去的模型（别名对过、规则或插件
    /// 换过的那个）和它的密钥映射。回答钩子用
    requested_model: String,
    sent_model: String,
    bridge: Option<crate::plugin::bridge::Bridge>,
    /// 这一次回答的回答钩子
    reply: Option<crate::plugin::reply::Stream>,
    /// 发出去的模型名和客户端要的不一样时，回答里的模型名换回客户端用的（见
    /// [`crate::answer_model`]）
    rename: Option<crate::answer_model::Body>,
}

impl Pipes {
    /// 这一帧报出去的事件（脱敏、内容过滤、工具调用审查、插件的运行）挂在哪个请求上：上游
    /// 此刻在回答的那一轮，没有就是这条连接
    fn event_id(&self) -> u64 {
        self.turns
            .as_ref()
            .and_then(turn::Turns::front_id)
            .unwrap_or(self.id)
    }
}

/// 这条连接在流量里怎么记（见 [`turn`]）。
pub(crate) enum Rows {
    /// 整条连接一行：Realtime 和别的路径的连接。升级时已经开始了，`ending` 是它欠着的结局。
    /// `realtime`：是 Realtime 的连接，这一行带着每一次回答的用量加起来的数
    Connection {
        id: u64,
        ending: Box<crate::ending::Ending>,
        realtime: bool,
    },
    /// 每一轮一行：Responses 的连接。连接本身不留行 —— 连不上上游的除外，那时按升级的那一刻
    /// （`upgraded`：用时从哪一刻算起、那一刻的 Unix 毫秒）补上这一行
    Turns {
        line: Arc<turn::Line>,
        upgraded: (std::time::Instant, u64),
    },
}

/// 接管一次升级。
///
/// 路由、鉴权都在调用方做完了 —— 这里把两条流接起来，并且**在每一帧
/// 上重新点一遍管线的保护**。
///
/// 整条连接一行的，路由事件在这里发：选中的那一家接没接下，要等和它握完手才知道。
/// **这条连接怎么断的，就是这个请求的结局**。每一条收场的路径都先报结局、再去关连接：关连接
/// 要等对面，而对面可能已经不在了。每一轮一行的（Responses），每一轮各报各的（见 [`turn`]）。
pub(crate) async fn proxy(
    state: AppState,
    client: WebSocket,
    upstream: Upstream,
    rules: Rules,
    rows: Rows,
    plugins: Option<Plugins>,
    naming: Option<Naming>,
) {
    let hop_started = std::time::Instant::now();
    let connected = connect(&upstream).await;
    // **连不上也要报。**和 HTTP 那条路一样，失败的时候恰恰最需要看这一跳；
    // 报在结局之前，存储层落库时手上才有它
    let name = &upstream.provider.name;
    // 每一轮一行的连接连上了：连接本身不留行，每一轮各有各的号（见 `turn`）。连不上的补上
    // 这一行，和整条连接一行的一样报
    let (id, ending, turns, realtime) = match (rows, &connected) {
        (
            Rows::Connection {
                id,
                mut ending,
                realtime,
            },
            _,
        ) => {
            ending.responded(101);
            (id, Some(*ending), None, realtime)
        }
        (Rows::Turns { line, .. }, Ok(_)) => (
            state.bus.next_id(),
            None,
            Some(turn::Turns::new(line)),
            false,
        ),
        (Rows::Turns { line, upgraded }, Err(_)) => {
            let (id, mut ending) = line.opener.open(turn::Opening {
                choice: &line.choice,
                to: (&line.provider, line.billing.into()),
                model: String::new(),
                session: None,
                input_estimate: None,
                started: upgraded.0,
                at_ms: upgraded.1,
            });
            ending.responded(101);
            (id, Some(ending), None, false)
        }
    };
    // 这一家接没接下这条连接，和 HTTP 那条路一跳的成败记在同一笔账上（见 `crate::health`）：
    // 连不上、回了 5xx 是失败，凭据被拒、限流按原因停用，请求本身的问题算这一家答上了。每一轮
    // 一行的连接接下了不记 —— 它的每一轮各记各的（见 `turn`）
    let change = match &connected {
        Ok(_) if turns.is_some() => None,
        Ok(_) => state.health.record_success(name),
        Err(NotConnected {
            status: Some(s), ..
        }) => match crate::failure::classify(
            *s,
            &axum::http::HeaderMap::new(),
            &[],
            crate::server::now_ms(),
        ) {
            crate::failure::Verdict::Failed(cause) => state.health.record_cause(name, cause),
            crate::failure::Verdict::ClientError => state.health.record_success(name),
        },
        // 地址、请求头写坏了：配置的事，不是这一家的
        Err(NotConnected {
            source: tw_api::FailureSource::Config,
            ..
        }) => None,
        Err(_) => state.health.record_failure(name),
    };
    crate::server::note_health(&state.bus, &state.health, name, change);
    if ending.is_some() {
        // 这一跳记发出去的模型名，和 HTTP 那条路一样。一条连接跑好几轮、每一帧写的模型可能
        // 不一样，升级时定得下来的只有每一帧都发的那个（指定模型、阶段一的改写，见
        // [`Naming::fixed`]）；Realtime 的连接是查询串里的模型对过的名字
        let model = upstream
            .model
            .clone()
            .or_else(|| naming.as_ref().and_then(|n| n.fixed(&state.catalog.load())));
        let attempt = match &connected {
            Ok(_) => crate::server::hop(
                name,
                model,
                tw_api::AttemptOutcome::Served,
                101,
                hop_started,
            ),
            Err(NotConnected {
                status: Some(s), ..
            }) => crate::server::hop(name, model, tw_api::AttemptOutcome::Status, *s, hop_started),
            Err(e) => crate::server::hop_failed(name, model, e.why.clone(), hop_started),
        };
        // 和 HTTP 那条路同一个规矩：没接下的不按那一家记账
        let billing = match &connected {
            Ok(_) => upstream.provider.billing,
            Err(_) => tw_config::Billing::PerToken,
        };
        state.bus.emit(tw_api::Event::RequestRouted {
            id,
            route: upstream.route,
            rule: upstream.rule,
            group: upstream.group,
            // 升级时就作用上的那几条（Realtime 的连接改写了查询串里的模型）
            rewritten_by: upstream.rewritten_by,
            denied_by: None,
            affinity: None,
            attempts: vec![attempt],
            billing: billing.into(),
        });
    }
    let up = match connected {
        Ok(up) => up,
        Err(e) => {
            let text = e.why.text.clone();
            if let Some(ending) = ending {
                ending.failed(e.source, e.why);
            }
            close_with(client, &text).await;
            return;
        }
    };
    let mut p = Pipes {
        ledger: tw_guard::redact::replace::Ledger::new(tw_guard::redact::replace::Scheme::SECRET),
        wall: rules
            .inspect_mode
            .detects()
            .then(|| tw_guard::tools::wall::Wall::new(rules.tools.clone())),
        response: None,
        dropping: false,
        rules,
        provider: upstream.provider.name,
        id,
        turns,
        realtime,
        plugins,
        naming,
        requested_model: String::new(),
        sent_model: String::new(),
        bridge: None,
        reply: None,
        rename: None,
    };
    pump(state, client, up, &mut p, ending).await;
}

/// 为什么没连上。
struct NotConnected {
    /// `x-thinkwatch-error` 那个词表：地址、头写坏了是 `config`，其余是
    /// `upstream`
    source: tw_api::FailureSource,
    /// 上游回了 101 以外的状态码：**它答了话，只是没接下这条连接**。根本
    /// 没连上的（地址不通、TLS 失败、握手中途断了）没有
    status: Option<u16>,
    why: Msg,
}

/// 按路由选中的那一家建连：拼请求、带上头、握手。
async fn connect(upstream: &Upstream) -> Result<Stream, NotConnected> {
    let config = |why: Msg| NotConnected {
        source: tw_api::FailureSource::Config,
        status: None,
        why,
    };
    let mut req = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        upstream.url.as_str(),
    )
    .map_err(|e| {
        config(msg!(
            "gw.ws.bad_url", detail = e =>
            "The upstream address is not a valid WebSocket address: {detail}"
        ))
    })?;
    for (name, value) in &upstream.headers {
        let parsed = (
            name.parse::<tokio_tungstenite::tungstenite::http::HeaderName>(),
            value.parse::<tokio_tungstenite::tungstenite::http::HeaderValue>(),
        );
        let (Ok(n), Ok(v)) = parsed else {
            return Err(config(msg!(
                "gw.ws.bad_header", header = name =>
                "The upstream header `{header}` contains characters a header may not carry."
            )));
        };
        req.headers_mut().insert(n, v);
    }
    dial(&upstream.url, req)
        .await
        .map_err(|(status, detail)| NotConnected {
            source: tw_api::FailureSource::Upstream,
            status,
            why: msg!(
                "gw.ws.connect_failed", detail = detail =>
                "The upstream WebSocket could not be connected: {detail}"
            ),
        })
}

/// 建连。没连上时给出上游回的状态码（它答了话的话）和原因。
///
/// **不用 tokio-tungstenite 自带的 `connect_async`。**那会带进另一套 TLS
/// 信任根，于是「数据面信任的证书」和「WS 信任的」变成两回事 —— 而那种
/// 不一致的表现是「HTTP 通、WS 报证书错误」，最难查的一类（L1 那边为了
/// 同一个理由也是复用这份配置）。
async fn dial(
    url: &str,
    req: tokio_tungstenite::tungstenite::handshake::client::Request,
) -> Result<Stream, (Option<u16>, String)> {
    let tls = url.starts_with("wss://");
    let hostport = url
        .trim_start_matches("wss://")
        .trim_start_matches("ws://")
        .split(['/', '?'])
        .next()
        .unwrap_or_default()
        .to_string();
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (
            h.to_string(),
            p.parse().unwrap_or(if tls { 443 } else { 80 }),
        ),
        _ => (hostport.clone(), if tls { 443 } else { 80 }),
    };
    let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| (None, format!("{host}:{port} could not be reached: {e}")))?;
    let io: Box<dyn Io> = if tls {
        let name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|_| (None, format!("{host} is not a valid TLS host name")))?;
        let conn = tokio_rustls::TlsConnector::from(crate::l1::tls_config());
        Box::new(
            conn.connect(name, tcp)
                .await
                .map_err(|e| (None, e.to_string()))?,
        )
    } else {
        Box::new(tcp)
    };
    let (s, _) = tokio_tungstenite::client_async(req, io)
        .await
        .map_err(|e| {
            let status = match &e {
                tokio_tungstenite::tungstenite::Error::Http(r) => Some(r.status().as_u16()),
                _ => None,
            };
            (status, e.to_string())
        })?;
    Ok(s)
}

trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

type Stream = tokio_tungstenite::WebSocketStream<Box<dyn Io>>;

/// 一条连接是怎么断的。
enum End {
    /// 客户端那一边收场了：发了关闭帧，或者走了（连接收掉了、写不过去了）。**客户端怎么走
    /// 都算这一种** —— 一次会话就是由客户端结束的，那是正常收场
    Closed,
    /// 上游收了连接：发了关闭帧，或者没有关闭帧就断开了。整条连接一行的，这也是收场；
    /// Responses 的连接上还没答完的那几轮是**失败**，上游没答完就走了 —— 记成客户端取消的话，
    /// 这一家内容之前断掉的那一轮不算它的失败，界面上也像是用户自己停下的
    UpstreamClosed,
    /// 上游那边出错断了，或者写不过去了
    Broke(Msg),
    /// 被防护切断了：回答里的工具调用命中了切断规则，或者客户端发来的一帧被内容过滤拒了
    Cut(Msg),
}

/// 在等准入的那一轮（见 [`turn::admit`]）：等到了交回这一轮和这一帧接下来要用的。
type Waiting = std::pin::Pin<
    Box<dyn std::future::Future<Output = (Result<turn::Turn, turn::NotAdmitted>, Next)> + Send>,
>;

/// 一帧 `response.create` 过了准入之后要用的：查过内容过滤的那一帧（删过的话是删过的样子）、
/// 客户端要的模型名、发给这一家的样子。
struct Next {
    text: String,
    requested: String,
    out: Outgoing,
}

/// 客户端的一帧处理完之后怎么办。
enum Step {
    Go,
    End(End),
}

async fn pump(
    state: AppState,
    client: WebSocket,
    up: Stream,
    p: &mut Pipes,
    mut ending: Option<crate::ending::Ending>,
) {
    let (mut c_tx, mut c_rx) = client.split();
    let (mut u_tx, mut u_rx) = up.split();
    // 一轮在等准入（上限、并发、这一家的位置）。**等的时候上游那一边照常转发**：前一轮的
    // 回答要接着交给客户端，它答完了，这一轮等的位置才空得出来
    let mut waiting: Option<Waiting> = None;
    // 等的时候客户端接着发来的帧：排在那一轮后面，轮到了按顺序处理
    let mut held: std::collections::VecDeque<Message> = Default::default();
    let end = 'pump: loop {
        while waiting.is_none()
            && let Some(m) = held.pop_front()
        {
            if let Step::End(end) =
                client_frame(&state, p, m, &mut c_tx, &mut u_tx, &mut waiting).await
            {
                break 'pump end;
            }
        }
        tokio::select! {
            msg = c_rx.next() => {
                let Some(Ok(m)) = msg else { break End::Closed };
                if waiting.is_some() {
                    if matches!(m, Message::Close(_)) { break End::Closed }
                    held.push_back(m);
                    continue;
                }
                if let Step::End(end) = client_frame(&state, p, m, &mut c_tx, &mut u_tx, &mut waiting).await {
                    break end;
                }
            }
            got = async { waiting.as_mut().expect("polled only while one is waiting").await },
                if waiting.is_some() => {
                waiting = None;
                if let Step::End(end) = admitted(&state, p, got, &mut c_tx, &mut u_tx).await {
                    break end;
                }
            }
            // 上游 → 客户端：先还原占位符，再过工具墙
            msg = u_rx.next() => {
                let m = match msg {
                    Some(Ok(m)) => m,
                    // 上游把连接收掉了，没有关闭帧也算它收了
                    None => break End::UpstreamClosed,
                    Some(Err(e)) => {
                    break End::Broke(msg!(
                        "gw.ws.upstream_broke", detail = e =>
                        "The upstream connection broke: {detail}"
                    ));
                }
                };
                let out = match m {
                    UpMsg::Text(t) => {
                        match upstream_text(&state, p, t.as_str(), &mut c_tx, &mut ending).await {
                            Flow::Sent => continue,
                            Flow::End(end) => break end,
                        }
                    }
                    UpMsg::Binary(b) => {
                        if let Some(e) = ending.as_mut() {
                            e.count(b.len());
                        }
                        Message::Binary(b)
                    }
                    UpMsg::Ping(b) => Message::Ping(b),
                    UpMsg::Pong(b) => Message::Pong(b),
                    UpMsg::Close(_) => break End::UpstreamClosed,
                    UpMsg::Frame(_) => continue,
                };
                // 发不给客户端，就是客户端已经走了
                if c_tx.send(out).await.is_err() { break End::Closed }
            }
        }
    };
    // **先报结局，再关连接。**关连接要等对面回话，而对面可能早就不在了。在等准入的那一轮
    // 开始了的话记成取消；在跑的几轮，上游断了、收了连接的是失败，客户端走了的是取消
    drop(waiting);
    if let Some(t) = p.turns.as_mut() {
        match &end {
            End::Broke(why) => t.fail_all(tw_api::FailureSource::Upstream, why.clone()),
            End::UpstreamClosed => t.fail_all(
                tw_api::FailureSource::Upstream,
                msg!(
                    "gw.ws.upstream_closed" =>
                    "The upstream closed the connection before the answer was complete."
                ),
            ),
            _ => t.clear(),
        }
    }
    if let Some(ending) = ending {
        match end {
            End::Closed | End::UpstreamClosed => ending.finished(101),
            End::Broke(why) => ending.failed(tw_api::FailureSource::Upstream, why),
            End::Cut(why) => ending.failed(tw_api::FailureSource::Denied, why),
        }
    }
    let _ = c_tx.close().await;
    let _ = u_tx.close().await;
}

type UpstreamSink = futures::stream::SplitSink<Stream, UpMsg>;

/// 客户端 → 上游的一帧：**和普通请求同一个脱敏函数**。Responses 的连接上，一帧
/// `response.create` 是一轮的开头（[`begin_turn`]），要过准入时放进 `waiting`。
async fn client_frame(
    state: &AppState,
    p: &mut Pipes,
    m: Message,
    c_tx: &mut ClientSink,
    u_tx: &mut UpstreamSink,
    waiting: &mut Option<Waiting>,
) -> Step {
    let out = match m {
        Message::Text(t) => {
            if p.turns.is_some()
                && let Some(frame) = create_frame(t.as_str())
            {
                return begin_turn(state, p, t.as_str(), frame, c_tx, waiting).await;
            }
            // 内容过滤在脱敏之前：看的是客户端的原话。删过的话，后面用删过的那一帧
            let text = match screen_frame(state, p, t.as_str()) {
                Ok(text) => text,
                Err(why) => {
                    let _ = c_tx
                        .send(Message::Text(format!("[ThinkWatch] {}", why.text).into()))
                        .await;
                    return Step::End(End::Cut(why));
                }
            };
            outbound(state, p, p.event_id(), text)
        }
        // 二进制不检查，也不假装检查过
        Message::Binary(b) => UpMsg::Binary(b),
        Message::Ping(b) => UpMsg::Ping(b),
        Message::Pong(b) => UpMsg::Pong(b),
        Message::Close(_) => return Step::End(End::Closed),
    };
    match u_tx.send(out).await {
        Ok(()) => Step::Go,
        Err(e) => Step::End(End::Broke(send_failed(e))),
    }
}

fn send_failed(e: impl std::fmt::Display) -> Msg {
    msg!(
        "gw.ws.send_failed", detail = e => "Sending to the upstream failed: {detail}"
    )
}

/// 是一帧 `response.create` 的话，解出来的样子
fn create_frame(text: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("response.create"))
}

/// 一轮的开头：一帧 `response.create`（`raw` 是客户端的原话，`frame` 是它解出来的样子）。
///
/// 位置和 HTTP 那条路一样：内容过滤先下结论（不报：结论挂在这一轮的号上报），定发给这一家的
/// 模型名和参数改写（[`Naming`]：规则拒绝了的照样留一行），然后交去准入（[`turn::admit`]）。
/// 插件的请求钩子、脱敏、发出在过了准入之后（[`admitted`]）。
async fn begin_turn(
    state: &AppState,
    p: &mut Pipes,
    raw: &str,
    frame: serde_json::Value,
    c_tx: &mut ClientSink,
    waiting: &mut Option<Waiting>,
) -> Step {
    let arrived = std::time::Instant::now();
    let at_ms = crate::server::now_ms();
    let (Some(naming), Some(turns)) = (p.naming.as_ref(), p.turns.as_ref()) else {
        return Step::Go;
    };
    let screening = {
        let s = &p.rules.screen;
        if s.mode.detects() {
            crate::guard::screen(s, tw_dialect::ir::Dialect::Responses, raw.as_bytes())
        } else {
            Default::default()
        }
    };
    // 删过的话，后面一律用删过的那一帧
    let (text, frame) = match &screening.body {
        Some(b) => {
            let text = String::from_utf8_lossy(b).into_owned();
            let frame = serde_json::from_str(&text).unwrap_or(frame);
            (text, frame)
        }
        None => (raw.to_string(), frame),
    };
    let reading = naming.read(&frame);
    let requested = reading.facts.model.clone();
    let input_estimate =
        matches!(reading.decoded, Some(Ok(_))).then_some(reading.facts.input_tokens);
    let fingerprint = crate::session::fingerprint(&frame);
    let line = turns.line.clone();
    // 别名对到这一家、密钥的模型范围继承时看的清单：这一帧（一次请求）用同一份
    let catalog = state.catalog.load();
    let out = match naming.frame(&catalog, &reading.facts) {
        Ok(out) => out,
        // 只是这一帧不发：替它回一个 `response.failed`，连接照常
        Err(no) => {
            tracing::info!(provider = %p.provider, why = %no.why.detail.text,
                "a WebSocket request was not sent");
            if let Some(by) = no.denied {
                turn::denied(
                    state,
                    &line,
                    by,
                    &no.why,
                    requested,
                    fingerprint.as_deref(),
                    input_estimate,
                    arrived,
                    at_ms,
                );
            }
            return reply_failed(c_tx, no.why).await;
        }
    };
    let admit = turn::Admit {
        state: state.clone(),
        line,
        rewritten_by: out.rewritten_by.clone(),
        requested: requested.clone(),
        sent: out.model.clone(),
        fingerprint,
        input_estimate,
        screening,
        arrived,
        at_ms,
    };
    let next = Next {
        text,
        requested,
        out,
    };
    *waiting = Some(Box::pin(async move { (turn::admit(admit).await, next) }));
    Step::Go
}

/// 一轮过了准入（或者没过）：过插件的请求钩子、脱敏，发给上游，排进在跑的那几轮里。没过的、
/// 插件拒绝的替它回一个 `response.failed`（被内容过滤、插件拒绝而切断的除外）。
async fn admitted(
    state: &AppState,
    p: &mut Pipes,
    (got, next): (Result<turn::Turn, turn::NotAdmitted>, Next),
    c_tx: &mut ClientSink,
    u_tx: &mut UpstreamSink,
) -> Step {
    let mut turn = match got {
        Ok(turn) => turn,
        Err(turn::NotAdmitted::Failed(err)) => {
            tracing::info!(provider = %p.provider, why = %err.detail.text,
                "a WebSocket request was not admitted");
            return reply_failed(c_tx, err).await;
        }
        Err(turn::NotAdmitted::Cut(why)) => {
            let _ = c_tx
                .send(Message::Text(format!("[ThinkWatch] {}", why.text).into()))
                .await;
            return Step::End(End::Cut(why));
        }
    };
    let text = match request(state, p, turn.id, next).await {
        Ok(text) => text,
        Err(Refusal::Cut(why)) => {
            turn.unsent(Vec::new(), tw_api::FailureSource::Denied, why.clone());
            let _ = c_tx
                .send(Message::Text(format!("[ThinkWatch] {}", why.text).into()))
                .await;
            return Step::End(End::Cut(why));
        }
        // 只是这一帧不发：替它回一个 `response.failed`，连接照常
        Err(Refusal::Frame(err)) => {
            tracing::info!(provider = %p.provider, why = %err.detail.text,
                "a WebSocket request was not sent");
            turn.unsent(Vec::new(), err.source.into(), err.detail.clone());
            return reply_failed(c_tx, err).await;
        }
    };
    let model = Some(p.sent_model.clone()).filter(|m| !m.is_empty() && *m != p.requested_model);
    let out = outbound(state, p, turn.id, text);
    turn.sent(model.clone());
    match u_tx.send(out).await {
        Ok(()) => {
            if let Some(t) = p.turns.as_mut() {
                t.push(turn);
            }
            Step::Go
        }
        Err(e) => {
            let why = send_failed(e);
            let hop = crate::server::hop_failed(
                &p.provider,
                model,
                why.clone(),
                std::time::Instant::now(),
            );
            turn.unsent(vec![hop], tw_api::FailureSource::Upstream, why.clone());
            Step::End(End::Broke(why))
        }
    }
}

/// 替没发出去的那一帧回一个 `response.failed`（见 [`failed_frame`]），连接照常
async fn reply_failed(c_tx: &mut ClientSink, err: GatewayError) -> Step {
    let failed = failed_frame(err, None);
    if c_tx.send(Message::Text(failed.into())).await.is_err() {
        return Step::End(End::Closed);
    }
    Step::Go
}

/// 发给上游之前的最后一步：出站脱敏，**和普通请求同一个函数、同一份全局规则**。客户端发来
/// 的一帧是一次请求，找到的挂在请求 `id` 上各报各的（一次最多报几个见
/// `crate::guard::REPORTED_MAX`）
fn outbound(state: &AppState, p: &mut Pipes, id: u64, text: String) -> UpMsg {
    let mode = p.rules.redact_mode;
    let found = crate::guard::find(mode, &p.rules.redact, text.as_bytes());
    if found.is_empty() {
        return UpMsg::Text(text.into());
    }
    state.bus.emit(tw_api::Event::SecretsFound {
        id,
        provider: p.provider.clone(),
        replaced: mode.acts(),
        items: crate::guard::items(&found, 0),
        at_ms: crate::server::now_ms(),
    });
    if !mode.acts() {
        return UpMsg::Text(text.into());
    }
    // 换的和报出去的是同一批：我们自己的占位符、base64 载荷不换
    let hits = crate::guard::hits(&text, &p.rules.redact);
    let r = tw_guard::redact::replace::apply(
        &text,
        &hits,
        std::mem::replace(
            &mut p.ledger,
            tw_guard::redact::replace::Ledger::new(tw_guard::redact::replace::Scheme::SECRET),
        ),
    );
    p.ledger = r.ledger;
    UpMsg::Text(r.text.into())
}

type ClientSink = futures::stream::SplitSink<WebSocket, Message>;

/// 上游的一帧文本处理完之后怎么办。
enum Flow {
    /// 该发的都发了（或者扣下了），接着收
    Sent,
    End(End),
}

/// 上游的一帧文本：还原占位符、回答钩子、工具墙，然后发给客户端。
///
/// Responses 的连接上它属于上游此刻在回答的那一轮（见 [`turn`]）：这一轮的结局按上游原话认
/// （用量、第一个 token、上游报的错），回答完了的那一帧交给客户端之后，这一轮收场、放掉它
/// 占着的。被工具墙切断的，这一轮记成拒绝。
///
/// **收了尾的那一轮又来的帧不算任何一轮的**（见 [`turn::Turns::late`]）：上游先报 `error`、
/// 再为同一次回答补一个 `response.failed` 时，后一帧照原样交给客户端，不让排在后面的那一轮
/// 背上它的失败。被切掉的那一轮已经替它发过 `response.failed`，它补发的不再发。
async fn upstream_text(
    state: &AppState,
    p: &mut Pipes,
    t: &str,
    c_tx: &mut ClientSink,
    ending: &mut Option<crate::ending::Ending>,
) -> Flow {
    let kind = frame_kind(t);
    // Realtime 的一次回答收了尾：它的用量加到这条连接的那一行上。**看的是上游原话**，和
    // 别的路一样（占位符不影响数字）
    if p.realtime
        && kind.as_deref() == Some("response.done")
        && let (Some(e), Some(u)) = (ending.as_mut(), realtime_usage(t))
    {
        e.add_usage(&u);
    }
    // 一次回答从开始到收尾的那几帧带着它的 id。只有它们要解第二遍
    let response = kind
        .as_deref()
        .filter(|k| lifecycle(k))
        .and_then(|_| response_id(t));
    let late = p
        .turns
        .as_mut()
        .zip(response.as_deref().zip(kind.as_deref()))
        .and_then(|(turns, (id, kind))| turns.late(id, kind));
    match late {
        Some(true) => return Flow::Sent,
        Some(false) => {}
        None => {
            if let Some(turn) = p.turns.as_mut().and_then(turn::Turns::front) {
                turn.upstream(t, kind.as_deref(), response.as_deref());
            }
        }
    }
    let flow = relay(state, p, t, kind.as_deref(), late.is_some(), c_tx, ending).await;
    if let Some(turns) = p.turns.as_mut() {
        match &flow {
            Flow::Sent if late.is_none() && ends_turn(kind.as_deref()) => turns.finish_front(),
            Flow::End(End::Cut(why)) => {
                turns.fail_front(tw_api::FailureSource::Denied, why.clone())
            }
            _ => {}
        }
    }
    flow
}

/// Realtime 的 `response.done` 里这一次回答的用量（`response.usage`）。和 Responses 一样，
/// `input_tokens` 里含着从缓存读的（`cached_tokens`），只是细分叫 `input_token_details`。
/// 语音、图片的 token 不分开：查价和别的请求一样按 token 的单价算。没有 `usage` 的是 None
fn realtime_usage(frame: &str) -> Option<tw_dialect::usage::Usage> {
    let v: serde_json::Value = serde_json::from_str(frame).ok()?;
    let u = v.get("response")?.get("usage")?;
    let n = |p: &str| {
        u.pointer(p)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    let cached = n("/input_token_details/cached_tokens");
    Some(tw_dialect::usage::Usage {
        input: n("/input_tokens").saturating_sub(cached),
        cache_read: cached,
        output: n("/output_tokens"),
        ..Default::default()
    })
}

/// 上游的这一帧（`type` 是 `kind`）是不是一次回答的结尾：完成、失败、没答完，或者一个错误
/// （没开始回答就出错的，上游只回一个 `error`）
fn ends_turn(kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some("response.completed" | "response.failed" | "response.incomplete" | "error")
    )
}

/// 一次回答从开始到收尾的那几帧（`type` 是 `kind`）：它们带着这次回答（`response.id`）
fn lifecycle(kind: &str) -> bool {
    matches!(
        kind,
        "response.created"
            | "response.queued"
            | "response.in_progress"
            | "response.completed"
            | "response.failed"
            | "response.incomplete"
    )
}

/// [`upstream_text`] 的转发那一半。`kind` 是这一帧的 `type`。`late` 是收了尾的那一次回答又来
/// 的一帧（见 [`turn::Turns::late`]）：照原样交给客户端（占位符照样还原、工具墙照样看），
/// 不碰此刻那一次回答的回答钩子和切掉的状态
async fn relay(
    state: &AppState,
    p: &mut Pipes,
    t: &str,
    kind: Option<&str>,
    late: bool,
    c_tx: &mut ClientSink,
    ending: &mut Option<crate::ending::Ending>,
) -> Flow {
    let restored = tw_guard::redact::replace::restore(t, &p.ledger);
    // 模型名换回客户端用的名称：一条消息是一个完整的 JSON，整条过一遍。排在回答钩子之前，
    // 和 HTTP 那条路一样
    let restored = match p.rename.as_mut() {
        Some(r) => {
            let mut out = r.feed(restored.as_bytes());
            out.extend(r.flush());
            String::from_utf8(out).unwrap_or(restored)
        }
        None => restored,
    };
    // 回答的边界：一次新的回答起一组回答钩子的实例；被切掉的那次剩下的帧不发
    let terminal = matches!(
        kind,
        Some("response.completed" | "response.failed" | "response.incomplete")
    );
    // 收了尾的那一次回答又来的一帧不是此刻这一次的边界
    if late {
        // 原样往下走
    } else if kind == Some("response.created") {
        p.response = response_id(&restored);
        p.dropping = false;
        // 回答钩子：这一次回答起一组实例
        if let Err(why) = start_reply(state, p).await {
            return fail_response(p, c_tx, why).await;
        }
    } else if p.dropping {
        if terminal {
            p.dropping = false;
        }
        return Flow::Sent;
    }
    // 回答钩子：一帧可能变成几帧，也可能先扣着
    let (outgoing, failed) = match p.reply.as_mut().filter(|_| !late) {
        None => (vec![restored], None),
        Some(s) => {
            let (out, mut err) = s.feed(as_sse(&restored).as_bytes()).await;
            let mut msgs = payloads(&out);
            if err.is_none() && terminal {
                let (more, e) = s.finish(false).await;
                msgs.extend(payloads(&more));
                err = e;
            }
            if terminal || err.is_some() {
                // 这一次回答完了：实例扔掉，记录交出去
                p.reply = None;
            }
            (msgs, err)
        }
    };
    for msg in outgoing {
        let hits = match p.wall.as_mut() {
            Some(w) => w.feed(as_sse(&msg).as_bytes()),
            None => Vec::new(),
        };
        // **和主管线一模一样的判据**：规则是切断 + 拦截档
        let acts = p.rules.inspect_mode.acts();
        // 头一个真要切的命中：告诉客户端的、结局里记的都是这一句
        let mut refusal: Option<Msg> = None;
        for h in &hits {
            let blocked = h.cut && acts;
            if blocked && refusal.is_none() {
                // 和 HTTP 那条路一样不说调用出自谁（见 relay 的 `wall_cut`）：回答钩子
                // 也能造、能改这一帧里的调用
                refusal = Some(msg!(
                    "gw.toolcall.connection_cut",
                    upstream = p.provider.clone(), tool = h.tool.clone(),
                    rule = h.rule.clone(), name = h.name.clone(),
                    why = h.why.clone() =>
                    "The answer contained a {tool} call that matched rule \
                     “{name}”{}, so the connection was cut.",
                    crate::server::because(&h.why)
                ));
            }
            // 命中的那一段是还原过的：报出去之前和留档一样打码
            let redaction = crate::bodies::Redaction {
                rules: p.rules.redact.clone(),
                ledger: p.ledger.clone(),
            };
            state.bus.emit(crate::server::flagged(
                p.event_id(),
                &p.provider,
                h,
                blocked,
                &redaction,
            ));
        }
        if let Some(why) = refusal {
            // **命中那一帧不发。**和 SSE 那条路同一条纪律：
            // 先判断再转发，而不是发完再说。告诉客户端的就是结局里
            // 那句带码的话，和内容过滤拒掉一帧时一样
            let _ = c_tx
                .send(Message::Text(format!("[ThinkWatch] {}", why.text).into()))
                .await;
            return Flow::End(End::Cut(why));
        }
        if let Some(e) = ending.as_mut() {
            e.count(msg.len());
        }
        // 发不给客户端，就是客户端已经走了
        if c_tx.send(Message::Text(msg.into())).await.is_err() {
            return Flow::End(End::Closed);
        }
    }
    if let Some(e) = failed {
        // 插件出错而策略是拒绝：切掉这一次回答，连接照常
        return fail_response(p, c_tx, e.detail).await;
    }
    Flow::Sent
}

/// 切掉这一次回答：替它发 `response.failed`，它剩下的帧不再发。这一轮的结局记成拒绝
async fn fail_response(p: &mut Pipes, c_tx: &mut ClientSink, why: Msg) -> Flow {
    if let Some(t) = p.turns.as_mut() {
        t.cut_front(why.clone());
    }
    let failed = failed_frame(GatewayError::denied(why), p.response.as_deref());
    p.dropping = true;
    p.reply = None;
    if c_tx.send(Message::Text(failed.into())).await.is_err() {
        return Flow::End(End::Closed);
    }
    Flow::Sent
}

/// 过了准入的一帧为什么还是不发（插件的请求钩子，见 [`plugin_request`]）。
enum Refusal {
    /// 切断这条连接，告诉客户端的是这句话：插件拒绝了这个请求，和内容过滤拒掉一帧一样
    Cut(Msg),
    /// 只是这一帧不发，替它回一个 `response.failed`（见 [`failed_frame`]），连接照常：插件换上的
    /// 别名这一家服务不了、密钥不让用插件换上的模型。下一帧要的可能就是能发的
    Frame(GatewayError),
}

/// 过了准入的一帧 `response.create`：过插件的请求钩子，写上发给这一家的模型名和规则的参数
/// 改写（[`Naming`] 在准入之前定好的，见 [`begin_turn`]）。返回要发给上游的那一帧：插件改过的
/// 话是改过的。`id` 是这一轮的号。
///
/// 这条路只有一跳：上游是这条连接连的那一家，插件的运行记在第 0 跳上。插件改过的那一版
/// **再查一遍内容过滤**，只报插件加进来的（客户端的原话在 [`begin_turn`] 查过了，见
/// [`screen_changed`]）。
async fn request(state: &AppState, p: &mut Pipes, id: u64, next: Next) -> Result<String, Refusal> {
    let Next {
        text,
        requested,
        out: Outgoing {
            model: sent, set, ..
        },
    } = next;
    let catalog = state.catalog.load();
    let (out, sent) = if p.plugins.is_some() {
        plugin_request(state, p, id, &catalog, &text, &requested, sent).await?
    } else {
        (text, sent)
    };
    // 和 HTTP 那条路一样，参数改写作用在插件改过的那一版上
    let out = rewrite(out, &sent, &set);
    p.rename =
        crate::answer_model::Rename::new(&requested, &sent).map(crate::answer_model::Rename::body);
    p.requested_model = requested;
    p.sent_model = sent;
    Ok(out)
}

/// 一次 `response.create` 过插件的请求钩子。`requested` 是这一帧写的模型名，`sent` 是发给
/// 这一家的（[`Naming::frame`]），插件的 `ctx.model` 就是它。返回要发出去的那一帧（插件改过
/// 的话是改过的）和发给这一家的模型名。
///
/// 插件换了模型名：插件写的是客户端那一侧的名字，**可以是别名**，和 HTTP 那条路一样先过密钥的
/// 模型范围、再按别名表对到这一家（`server::pipeline::plug`）；密钥不让用、这一家服务不了这个
/// 别名的，这一帧不发。
async fn plugin_request(
    state: &AppState,
    p: &mut Pipes,
    id: u64,
    catalog: &tw_engine::Catalog,
    text: &str,
    requested: &str,
    sent: String,
) -> Result<(String, String), Refusal> {
    let Some(pc) = p.plugins.as_ref() else {
        return Ok((text.to_string(), sent));
    };
    let body = bytes::Bytes::copy_from_slice(text.as_bytes());
    let mut hook = crate::plugin::request::Hook::new(
        &pc.set,
        p.rules.redact.clone(),
        tw_dialect::ir::Dialect::Responses,
        "/responses",
        pc.client.as_deref(),
        &body,
    );
    let to = crate::plugin::request::Target {
        upstream: &p.provider,
        model: &sent,
        requested_model: requested,
        attempt: 0,
    };
    let plugged = match hook.attempt(&pc.pool, &to).await {
        Ok(plugged) => plugged,
        Err(refused) => {
            crate::plugin::request::record(state, id, &refused.runs);
            return Err(Refusal::Cut(refused.why));
        }
    };
    crate::plugin::request::record(state, id, &plugged.runs);
    p.bridge = plugged.bridge;
    let Some(c) = plugged.changed else {
        return Ok((text.to_string(), sent));
    };
    let sent = match (&c.renamed, p.naming.as_ref()) {
        (Some(r), Some(n)) => {
            // 插件换上的名字，这把密钥用不用得了：和 HTTP 那条路同一关、同一句
            // （`server::pipeline::plug` 的 `allowed`）。插件写的是客户端那一侧的名字
            if !n.allows(catalog, &r.model, false) {
                return Err(Refusal::Frame(GatewayError::request(msg!(
                    "gw.plugin.model_not_allowed",
                    plugin = r.by.clone(), model = r.model.clone(), key = n.client.clone() =>
                    "Plugin `{plugin}` changed the model to {model}, which gateway key `{key}` may not \
                     use, so the request was not sent."
                ))));
            }
            n.resolve(catalog, &r.model).ok_or_else(|| {
                // 和 HTTP 那条路同一句（`server::pipeline::plug` 的 `sent_name`）：这条路只有
                // 一跳，「不发往那一家」就是这一帧不发
                Refusal::Frame(GatewayError::request(msg!(
                    "gw.plugin.alias_unserved",
                    plugin = r.by.clone(), model = r.model.clone(), upstream = p.provider.clone() =>
                    "Plugin `{plugin}` changed the model to the alias {model}, and upstream \
                     `{upstream}` offers none of its models, so the request was not sent there."
                )))
            })?
        }
        _ => sent,
    };
    let out = screen_changed(
        state,
        p,
        id,
        text,
        String::from_utf8_lossy(&c.body).into_owned(),
    )
    .map_err(Refusal::Cut)?;
    Ok((out, sent))
}

/// 这一帧发出去的样子：模型名写成 `model`，规则的参数改写（`set` 的最大输出、关思考）照 HTTP
/// 那条路直通时的写法改（[`crate::forward::apply_set`]，按 Responses 的字段名）。`set` 里的
/// 模型名不看：发出去的是 `model`。什么都不用改（模型名已经是它或者 `model` 是空的，又没有
/// 参数改写）就一个字节都不动
fn rewrite(text: String, model: &str, set: &tw_engine::SetAction) -> String {
    let mut set = tw_engine::SetAction {
        model: None,
        ..set.clone()
    };
    if !model.is_empty() {
        let written = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("model")?.as_str().map(str::to_string));
        if written.as_deref() != Some(model) {
            set.model = Some(model.to_string());
        }
    }
    if set.is_empty() {
        return text;
    }
    let body = bytes::Bytes::from(text);
    let out = crate::forward::apply_set(&body, &set, Some(tw_dialect::ir::Dialect::Responses));
    String::from_utf8(Vec::from(out))
        .unwrap_or_else(|_| String::from_utf8_lossy(&body).into_owned())
}

/// 插件改过的那一帧再查一遍内容过滤：**只报插件加进来的**（见 [`crate::guard::rescreen`]）。
/// `before` 是插件拿到的那一帧，`after` 是插件改过的。两份都是 `response.create`，按
/// Responses 的消息结构查，和 [`screen_frame`] 一样。
///
/// 返回要发出去的那一帧：处置档下插件加进来的字命中了删除规则的，是删过的样子。要拒绝时
/// 是告诉客户端的那句话
fn screen_changed(
    state: &AppState,
    p: &Pipes,
    id: u64,
    before: &str,
    after: String,
) -> Result<String, Msg> {
    let s = &p.rules.screen;
    if !s.mode.detects() {
        return Ok(after);
    }
    let sc = crate::guard::rescreen(
        s,
        tw_dialect::ir::Dialect::Responses,
        before.as_bytes(),
        after.as_bytes(),
    );
    if let Some(why) = crate::guard::report(&state.bus, id, &p.provider, &sc) {
        return Err(why);
    }
    Ok(match sc.body {
        Some(b) => String::from_utf8(b.to_vec()).unwrap_or(after),
        None => after,
    })
}

/// 这一次回答起回答钩子的实例。范围里没有就什么都不做；起不来而策略是拒绝时是那句话
async fn start_reply(state: &AppState, p: &mut Pipes) -> Result<(), Msg> {
    p.reply = None;
    let Some(pc) = p.plugins.as_ref() else {
        return Ok(());
    };
    let bridge = p
        .bridge
        .clone()
        .unwrap_or_else(|| crate::plugin::bridge::Bridge::new(p.rules.redact.clone()));
    let ctx = crate::plugin::reply::ReplyCtx {
        dialect: tw_dialect::ir::Dialect::Responses,
        client: pc.client.as_deref(),
        model: &p.sent_model,
        requested_model: &p.requested_model,
        upstream: &p.provider,
        request_id: p.event_id(),
        attempt: 0,
    };
    match crate::plugin::reply::Chain::start(state, &pc.set, bridge, &ctx).await {
        Ok(Some(chain)) => {
            p.reply = Some(crate::plugin::reply::Stream::new(
                chain,
                crate::plugin::reply::Framing::Sse,
            ));
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(e) => Err(e.detail),
    }
}

/// 回答钩子交回来的 SSE 拆回一帧一帧的消息
fn payloads(out: &[u8]) -> Vec<String> {
    let mut d = tw_dialect::frame::Decoder::default();
    let mut frames = d.feed(out);
    frames.extend(d.flush());
    frames.into_iter().map(|f| f.data).collect()
}

/// 一帧的 `type`：Responses 的事件都带着它（`response.created` …）。不是 JSON 的是 None
fn frame_kind(frame: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(frame).ok()?;
    v.get("type")?.as_str().map(str::to_string)
}

/// `response.created` 里那次回答的 id
fn response_id(frame: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(frame).ok()?;
    v.pointer("/response/id")?.as_str().map(str::to_string)
}

/// 替被切掉的那次回答（或者没发出去的那一帧）发的 `response.failed`：和 SSE 那条路同一个
/// 形状（`tw_dialect` 的错误帧），id 换成这次回答的。没发出去的那一帧没有回答，id 是新的
fn failed_frame(err: GatewayError, response: Option<&str>) -> String {
    let until_reset = err.retry.is_some_and(|r| r.until_reset);
    let sse = err
        .in_dialect(tw_dialect::ir::Dialect::Responses)
        .sse_frame();
    let Some(mut v) = tw_dialect::frame::parse(sse.as_bytes())
        .and_then(|f| serde_json::from_str::<serde_json::Value>(&f.data).ok())
    else {
        return sse;
    };
    if let Some(id) = response {
        v["response"]["id"] = serde_json::Value::String(id.to_string());
    }
    // 密钥这一期的上限用完了：和 HTTP 那条路的 OpenAI 格式一样写成额度用完（见
    // `crate::error::Retry`）。Codex 按 `code` 决定退不退避，认这个码的直接停下来告诉用户
    if until_reset {
        v["response"]["error"]["code"] = serde_json::Value::String("insufficient_quota".into());
    }
    v.to_string()
}

/// 客户端发来的一帧过一遍内容过滤：处置档下该拒的话是告诉客户端的那句话，否则是要发
/// 出去的那一帧（删过的话是删过的样子）。命中的挂在 [`Pipes::event_id`] 上报。
///
/// Codex 在 WS 上发的是 `{"type":"response.create", …}`，其余字段就是一个 Responses
/// 请求：**按消息结构看**，和 HTTP 那条路一样只看调用方的消息、删也只删那里（见
/// [`crate::guard::screen`]）。别的帧只用码位规则查整段原文（见
/// [`crate::guard::screen_raw`]）。Responses 的连接上一帧 `response.create` 是一轮的开头，
/// 在 [`begin_turn`] 里查，结论挂在那一轮上报。
fn screen_frame(state: &AppState, p: &Pipes, text: &str) -> Result<String, Msg> {
    let s = &p.rules.screen;
    if !s.mode.detects() {
        return Ok(text.to_string());
    }
    let request = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .is_some_and(|v| v.get("type").and_then(|t| t.as_str()) == Some("response.create"));
    let sc = if request {
        crate::guard::screen(s, tw_dialect::ir::Dialect::Responses, text.as_bytes())
    } else {
        crate::guard::screen_raw(s, text)
    };
    if let Some(why) = crate::guard::report(&state.bus, p.event_id(), &p.provider, &sc) {
        return Err(why);
    }
    Ok(match sc.body {
        Some(b) => String::from_utf8(b.to_vec()).unwrap_or_else(|_| text.to_string()),
        None => text.to_string(),
    })
}

/// 把一帧喂成工具墙认得的样子。
///
/// 工具墙是按 SSE 写的（`data: {…}` 行），而 WS 上一帧就是一个 JSON
/// 对象。包一层比给工具墙开第二个入口好 —— **两个入口迟早会在「哪些
/// 规则跑」这件事上分叉**，而那时只有一条路是安全的。
///
/// 桥接本身可能已经在转发 SSE 文本（sub2api 那条链路是 WS ↔ HTTP/SSE），
/// 所以已经是那个形状的就原样过去。
///
/// > **这里的帧形状没有实测过。**手上没有跑着的 sub2api WS 桥，所以
/// > 两种形状都喂 —— 猜错一种的代价是漏检，而漏检正是这一层要防的。
fn as_sse(frame: &str) -> String {
    if frame.lines().any(|l| l.starts_with("data: ")) {
        return frame.to_string();
    }
    format!("data: {frame}\n\n")
}

async fn close_with(client: WebSocket, why: &str) {
    let mut c = client;
    // **说清楚为什么。**一个默默断掉的 WebSocket，客户端只会显示
    // 「连接已关闭」，而用户完全无从下手
    let _ = c
        .send(Message::Text(format!("[ThinkWatch] {why}").into()))
        .await;
    let _ = c.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_upgrade_request_is_recognised_case_insensitively() {
        let mut h = axum::http::HeaderMap::new();
        assert!(!is_upgrade(&h));
        h.insert("upgrade", "WebSocket".parse().unwrap());
        // 只有 upgrade 不算 —— 两个头都要在
        assert!(!is_upgrade(&h));
        h.insert("connection", "keep-alive, Upgrade".parse().unwrap());
        assert!(is_upgrade(&h));
    }

    #[test]
    fn the_scheme_follows_the_base_url() {
        assert_eq!(
            upstream_url(
                "https://a.example.com",
                "/backend-api/codex/responses",
                None
            ),
            "wss://a.example.com/backend-api/codex/responses"
        );
        assert_eq!(
            upstream_url("http://127.0.0.1:8080/", "/x", Some("a=1")),
            "ws://127.0.0.1:8080/x?a=1"
        );
        // 空 query 不该留一个光秃秃的问号
        assert_eq!(upstream_url("http://h", "/x", Some("")), "ws://h/x");
    }

    #[test]
    fn a_realtime_answer_reports_its_usage_with_the_cache_read_split_out() {
        let done = r#"{"type":"response.done","response":{"id":"r","status":"completed","usage":{"total_tokens":253,"input_tokens":132,"output_tokens":121,"input_token_details":{"text_tokens":119,"audio_tokens":13,"cached_tokens":64},"output_token_details":{"text_tokens":30,"audio_tokens":91}}}}"#;
        let u = realtime_usage(done).unwrap();
        assert_eq!((u.input, u.cache_read, u.output), (68, 64, 121));
        assert_eq!(
            realtime_usage(r#"{"type":"response.done","response":{"id":"r"}}"#),
            None
        );
    }

    #[test]
    fn the_realtime_model_is_read_from_and_written_into_the_query() {
        assert!(realtime("/v1/realtime"));
        assert!(realtime("/realtime/"));
        assert!(!realtime("/v1/responses"));
        assert_eq!(
            query_model(Some("intent=chat&model=gpt-realtime%2Dmini")).as_deref(),
            Some("gpt-realtime-mini")
        );
        assert_eq!(query_model(Some("model=a+b%")).as_deref(), Some("a b%"));
        assert_eq!(query_model(Some("model=&x=1")), None);
        assert_eq!(query_model(Some("models=x")), None);
        assert_eq!(query_model(None), None);
        // 只换 `model` 那一项，别的项一个字节都不动
        assert_eq!(
            with_query_model("x=%2F&model=voice&y", "us.anthropic.v1:0/x y"),
            "x=%2F&model=us.anthropic.v1:0%2Fx%20y&y"
        );
    }

    #[test]
    fn the_gateway_key_in_the_query_never_reaches_the_upstream() {
        assert_eq!(
            upstream_url("https://h", "/x", Some("key=tw-secret&alt=sse")),
            "wss://h/x?alt=sse"
        );
        assert_eq!(
            upstream_url("https://h", "/x", Some("key=tw-secret")),
            "wss://h/x"
        );
        // 只剔名字正好是 `key` 的那一项
        assert_eq!(
            upstream_url("http://h", "/x", Some("monkey=1&key&keys=2")),
            "ws://h/x?monkey=1&keys=2"
        );
    }
}
