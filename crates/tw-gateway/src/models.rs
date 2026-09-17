//! 每个上游提供哪些模型：向上游要来的、配置里手写的，以及它们是什么时候、
//! 怎么来的。
//!
//! # 两份东西
//!
//! - **目录（[`Directory`]）**记每个上游的原始答案：列出了什么、上次什么时候
//!   问的、没问到的话为什么。它跨重载存活。
//! - **汇总（`tw_engine::Catalog`）**从目录和当前配置推出来：停用的上游不算，
//!   启用范围（`models_only`）之外的模型不算。`/v1/models`、准入、挑候选
//!   都只看它。
//!
//! 配置一变，汇总马上按新配置重算；只有地址、凭据、协议、代理变了的上游
//! 才需要重新去问，其余的答案原样留着。以前改一次配置，汇总就换回只含手写
//! 清单的那份：已经问到的模型全部消失，新加的上游要等到第二天的定时刷新。
//!
//! **向上游问是后台的事。**探测零成本，但要打网络：它不挡启动，`/v1/models`
//! 也不现问 —— 每次有人列模型就去打一遍上游，会把一个本该零成本的端点变成
//! 一串网络往返。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use crate::probe::ModelList;
use crate::server::{AppState, Runtime, now_ms};

/// 多久向每个上游重新问一次。
const REFRESH_EVERY: Duration = Duration::from_secs(24 * 3600);
/// 没问到（连不上、密钥被拒）之后多久再试。
const RETRY_AFTER: Duration = Duration::from_secs(3600);
/// 多久看一次有没有到时间的。看一次只是比较时间，不联网。
///
/// **不是睡满一天再醒。**笔记本合盖时单调时钟不走，睡「24 小时」可能睡上
/// 好几天；按墙上时间一轮轮比，开盖之后下一轮就会去问。
const TICK: Duration = Duration::from_secs(10 * 60);

/// 一个上游的模型清单从哪儿来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// 上游自己列出来的
    Discovered,
    /// 上游没给出清单，用配置里手写的 `models`
    Manual,
    /// 都没有。**不知道它有什么**，不是它什么都没有
    None,
}

impl Source {
    pub fn slug(&self) -> &'static str {
        match self {
            Source::Discovered => "discovered",
            Source::Manual => "manual",
            Source::None => "none",
        }
    }
}

/// 一个上游的模型清单。
#[derive(Debug, Clone)]
pub struct Listing {
    pub source: Source,
    /// 清单本身。**还没按启用范围过滤**
    pub models: Vec<String>,
    /// 最近一次向上游问的时间。还没问过是空
    pub checked_at_ms: Option<u64>,
    /// 没从上游拿到清单的原因
    pub error: Option<String>,
}

/// 最近一次向上游问的结果。
#[derive(Debug, Clone)]
enum Answer {
    /// 还没问过
    Pending,
    /// 列出来了
    Listed(Vec<String>),
    /// 问到了，但上游没给出清单：没有这个接口、格式认不出、空的
    NoList(String),
    /// 没问到：连不上、密钥被拒、取不到密钥
    Failed(String),
}

#[derive(Debug, Clone)]
struct Entry {
    /// 问的时候这家长什么样。地址、凭据、协议、代理变了，答案就作废
    identity: String,
    answer: Answer,
    checked_at_ms: Option<u64>,
}

#[derive(Debug, Default)]
pub struct Directory {
    entries: Mutex<HashMap<String, Entry>>,
    /// 有上游等着去问
    wake: tokio::sync::Notify,
}

/// 决定答案还作不作数的那几样。
///
/// **OAuth 只看账号，不看 token**：refresh token 轮换一次就重问一遍，问到的
/// 是同一份清单。
fn identity(cfg: &tw_config::Config, p: &tw_config::Provider) -> String {
    let key = match &p.key {
        tw_config::Secret::Literal(v) => v.clone(),
        tw_config::Secret::OAuth { oauth } => format!(
            "oauth|{}|{}",
            oauth.endpoint,
            oauth.client_id.as_deref().unwrap_or_default()
        ),
        tw_config::Secret::Unknown(_) => String::new(),
    };
    let raw = format!(
        "{}|{key}|{:?}|{}",
        p.base_url,
        p.effective_protocol(),
        crate::server::proxy_shape(cfg, p)
    );
    // **存指纹，不存原文** —— 这张表不该多出一份密钥
    blake3::hash(raw.as_bytes()).to_hex().to_string()
}

impl Directory {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 配置换了之后对一遍：删掉的上游忘掉，新加的、身份变了的等着去问。
    /// 返回有没有要去问的。
    pub(crate) fn reconcile(&self, cfg: &tw_config::Config) -> bool {
        let mut entries = self.lock();
        entries.retain(|name, _| cfg.providers.iter().any(|p| &p.name == name));
        let mut pending = false;
        for p in &cfg.providers {
            let id = identity(cfg, p);
            if entries.get(&p.name).is_some_and(|e| e.identity == id) {
                continue;
            }
            entries.insert(
                p.name.clone(),
                Entry {
                    identity: id,
                    answer: Answer::Pending,
                    checked_at_ms: None,
                },
            );
            pending = true;
        }
        pending
    }

    pub(crate) fn wake(&self) {
        self.wake.notify_one();
    }

    /// 这个上游的模型清单。
    pub fn listing(&self, p: &tw_config::Provider) -> Listing {
        let entry = self.lock().get(&p.name).cloned();
        listing_of(entry.as_ref(), p)
    }

    /// 按目录和配置推出汇总，并换进 `into`。
    ///
    /// **推算和换入在同一把锁里，配置在锁里现取。**否则一次探测回来和一次
    /// 配置重载交错时，后换入的可能是按旧配置推出来的那份。
    pub(crate) fn publish(
        &self,
        config: impl Fn() -> std::sync::Arc<tw_config::Config>,
        into: &arc_swap::ArcSwap<tw_engine::Catalog>,
    ) {
        let entries = self.lock();
        let cfg = config();
        let sources: Vec<tw_engine::ProviderModels> = cfg
            .providers
            .iter()
            // 停用的上游不提供任何东西
            .filter(|p| !p.disabled)
            .map(|p| {
                let l = listing_of(entries.get(&p.name), p);
                tw_engine::ProviderModels {
                    provider: p.name.clone(),
                    protocol: p
                        .effective_protocol()
                        .map(|x| format!("{x:?}"))
                        // 猜不出协议时按 Anthropic 算 —— 和转发时的默认一致
                        // （forward::apply_credential）。两处不一致会让「列出来
                        // 了但发过去 401」变成可能。
                        .unwrap_or_else(|| "Anthropic".to_string()),
                    known: l.source != Source::None,
                    models: l.models.into_iter().filter(|m| p.uses_model(m)).collect(),
                }
            })
            .collect();
        into.store(std::sync::Arc::new(tw_engine::Catalog::build(&sources)));
    }

    /// 到时间该去问的上游。
    fn due(&self, cfg: &tw_config::Config, now: u64) -> Vec<String> {
        let entries = self.lock();
        cfg.providers
            .iter()
            // 停用的上游用不着：打开它时身份不变，也会在到时间时补上
            .filter(|p| !p.disabled)
            .filter(|p| match entries.get(&p.name) {
                None => true,
                Some(e) => match (&e.answer, e.checked_at_ms) {
                    (Answer::Pending, _) | (_, None) => true,
                    (Answer::Failed(_), Some(at)) => {
                        now.saturating_sub(at) >= RETRY_AFTER.as_millis() as u64
                    }
                    (_, Some(at)) => now.saturating_sub(at) >= REFRESH_EVERY.as_millis() as u64,
                },
            })
            .map(|p| p.name.clone())
            .collect()
    }

    /// 记下一次答案。**问的时候那家的身份已经变了的话，这个答案作废** ——
    /// 配置重载已经把它排进了下一轮。
    fn record(&self, name: &str, identity: &str, answer: Answer, at: u64) {
        let mut entries = self.lock();
        if let Some(e) = entries.get_mut(name)
            && e.identity == identity
        {
            e.answer = answer;
            e.checked_at_ms = Some(at);
        }
    }
}

fn listing_of(entry: Option<&Entry>, p: &tw_config::Provider) -> Listing {
    let checked_at_ms = entry.and_then(|e| e.checked_at_ms);
    let error = entry.and_then(|e| match &e.answer {
        Answer::NoList(why) | Answer::Failed(why) => Some(why.clone()),
        Answer::Pending | Answer::Listed(_) => None,
    });
    match entry.map(|e| &e.answer) {
        Some(Answer::Listed(models)) => Listing {
            source: Source::Discovered,
            models: models.clone(),
            checked_at_ms,
            error: None,
        },
        // 问不到就用手写的兜底。**两者不合并** —— 合并的话，用户删掉一个
        // 上游不再提供的模型时会发现它删不掉
        _ if !p.models.is_empty() => Listing {
            source: Source::Manual,
            models: p.models.clone(),
            checked_at_ms,
            error,
        },
        _ => Listing {
            source: Source::None,
            models: Vec::new(),
            checked_at_ms,
            error,
        },
    }
}

/// 问一家上游有哪些模型。
async fn ask(state: &AppState, rt: &Runtime, p: &tw_config::Provider) -> Answer {
    // 用这一家自己的 client：它带着该走的代理，换 OAuth token 也要走那条路
    let http = rt.clients.get(&p.name).unwrap_or(&state.http);
    let key = match state.key_for(p, http).await {
        Ok(k) => k,
        Err(e) => return Answer::Failed(format!("取不到凭据：{e}")),
    };
    let r = crate::probe::probe(http, &p.base_url, &key, p.effective_protocol()).await;
    if !r.ok {
        return Answer::Failed(r.error.unwrap_or_else(|| "检测失败".to_string()));
    }
    match r.models {
        ModelList::Listed { models } => Answer::Listed(models),
        ModelList::NotImplemented { status } => {
            Answer::NoList(format!("上游没有提供模型列表接口（HTTP {status}）"))
        }
        ModelList::Unrecognized { .. } => {
            Answer::NoList("上游返回的模型列表格式无法识别".to_string())
        }
        ModelList::Empty => Answer::NoList("上游返回的模型列表为空".to_string()),
    }
}

/// 去问这几家，记下答案，重算汇总。**几家一起问** —— 探测互不干扰，而一家
/// 连不上要等满超时，挨个问会让排在后面的都跟着等。
async fn refresh(state: &AppState, names: &[String]) {
    let rt = state.runtime();
    let asks = names.iter().filter_map(|name| {
        let p = rt.config.providers.iter().find(|p| &p.name == name)?;
        let id = identity(&rt.config, p);
        let rt = &rt;
        Some(async move { (p, id, ask(state, rt, p).await) })
    });
    for (p, id, answer) in futures::future::join_all(asks).await {
        match &answer {
            Answer::Listed(models) => {
                tracing::debug!(provider = %p.name, models = models.len(), "模型清单已获取")
            }
            Answer::NoList(why) | Answer::Failed(why) => {
                tracing::info!(provider = %p.name, "没有获取到模型清单：{why}")
            }
            Answer::Pending => {}
        }
        state.models.record(&p.name, &id, answer, now_ms());
    }
    state.publish_catalog();
}

/// 马上向所有上游问一遍。
pub async fn refresh_all(state: &AppState) {
    let names: Vec<String> = state
        .config()
        .providers
        .iter()
        .map(|p| p.name.clone())
        .collect();
    refresh(state, &names).await;
}

/// 马上问这一家，返回它现在的清单。**停用了也问** —— 这是用户点的。
pub async fn refresh_one(state: &AppState, name: &str) -> Option<Listing> {
    refresh(state, &[name.to_string()]).await;
    let cfg = state.config();
    let p = cfg.providers.iter().find(|p| p.name == name)?;
    Some(state.models.listing(p))
}

/// 后台：启动时问一遍，之后每天一遍；配置里新加、改了的上游随时补问。
///
/// **`url-test` 的垫底跟着它一起跑**：两者都是「启动时打一次网络、之后靠
/// 真实流量」，而且都不该挡住启动。
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut seeded_at: Option<u64> = None;
        loop {
            let now = now_ms();
            if seeded_at.is_none_or(|t| now.saturating_sub(t) >= REFRESH_EVERY.as_millis() as u64) {
                crate::server::seed_latency(&state).await;
                seeded_at = Some(now);
            }
            let due = state.models.due(&state.config(), now);
            if !due.is_empty() {
                refresh(&state, &due).await;
            }
            tokio::select! {
                _ = tokio::time::sleep(TICK) => {}
                _ = state.models.wake.notified() => {}
            }
        }
    });
}

/// 为什么跳过一个候选上游。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// 停用了
    Disabled,
    /// 这个模型不在它的启用范围里
    OutOfScope,
    /// 它的模型清单里没有这个模型
    NotOffered,
}

impl Skip {
    pub fn slug(&self) -> &'static str {
        match self {
            Skip::Disabled => "disabled",
            Skip::OutOfScope => "out_of_scope",
            Skip::NotOffered => "not_offered",
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Skip::Disabled => "已停用",
            Skip::OutOfScope => "该模型不在启用范围内",
            Skip::NotOffered => "不提供该模型",
        }
    }
}

/// 路由选出的候选里，哪些能服务这个请求。
#[derive(Debug, Clone, Default)]
pub struct Serving {
    pub usable: Vec<String>,
    pub skipped: Vec<(String, Skip)>,
}

impl Serving {
    /// 一个都不剩时给客户端的那句话。
    pub fn explain(&self, model: &str) -> crate::GatewayError {
        let why = self
            .skipped
            .iter()
            .map(|(p, s)| format!("{p} {}", s.label()))
            .collect::<Vec<_>>()
            .join("；");
        // 全是停用：请求没问题，是配置里能用的上游都关掉了
        if self.skipped.iter().all(|(_, s)| *s == Skip::Disabled) {
            crate::GatewayError::config(format!("路由选中的上游都已停用：{why}"))
        } else {
            crate::GatewayError::new(
                crate::error::Source::Request,
                format!("没有可用的上游提供模型 `{model}`：{why}"),
            )
        }
    }
}

/// 在路由选出的候选里去掉服务不了这个请求的上游。
///
/// **没有模型清单的上游不跳过**：不知道它有什么，不等于它没有。`model`
/// 是空的（请求体解析不了）时只看停用。
pub fn serving(
    cfg: &tw_config::Config,
    catalog: &tw_engine::Catalog,
    candidates: &[String],
    model: &str,
) -> Serving {
    let mut out = Serving::default();
    for name in candidates {
        let Some(p) = cfg.providers.iter().find(|p| &p.name == name) else {
            // 配置里没有这一家：留给尝试那一步报出来
            out.usable.push(name.clone());
            continue;
        };
        let skip = if p.disabled {
            Some(Skip::Disabled)
        } else if model.is_empty() {
            None
        } else if !p.uses_model(model) {
            Some(Skip::OutOfScope)
        } else if catalog.offers(name, model) == Some(false) {
            Some(Skip::NotOffered)
        } else {
            None
        };
        match skip {
            Some(s) => out.skipped.push((name.clone(), s)),
            None => out.usable.push(name.clone()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str) -> tw_config::Provider {
        tw_config::Provider {
            name: name.into(),
            base_url: format!("https://{name}.example"),
            key: "sk".into(),
            ..Default::default()
        }
    }

    fn cfg(providers: Vec<tw_config::Provider>) -> tw_config::Config {
        tw_config::Config {
            providers,
            ..Default::default()
        }
    }

    fn published(d: &Directory, c: &tw_config::Config) -> tw_engine::Catalog {
        let into = arc_swap::ArcSwap::from_pointee(tw_engine::Catalog::default());
        let c = std::sync::Arc::new(c.clone());
        d.publish(|| c.clone(), &into);
        (*into.load_full()).clone()
    }

    #[test]
    fn an_edit_that_does_not_change_how_to_reach_an_upstream_keeps_its_answer() {
        let d = Directory::default();
        let mut c = cfg(vec![provider("a"), provider("b")]);
        assert!(d.reconcile(&c));
        for p in &c.providers {
            d.record(
                &p.name,
                &identity(&c, p),
                Answer::Listed(vec![format!("{}-m", p.name)]),
                1,
            );
        }
        // 改计费方式、启用范围：不用重问
        c.providers[0].billing = Some(tw_config::Billing::Free);
        c.providers[0].models_only = Some(vec!["a-*".into()]);
        assert!(!d.reconcile(&c));
        assert_eq!(d.listing(&c.providers[0]).models, ["a-m"]);
        // 改地址：重问，旧答案不再作数
        c.providers[1].base_url = "https://elsewhere.example".into();
        assert!(d.reconcile(&c));
        let l = d.listing(&c.providers[1]);
        assert_eq!(l.source, Source::None);
        assert_eq!(d.due(&c, 2), ["b"]);
        // 删掉的上游忘掉
        c.providers.remove(0);
        d.reconcile(&c);
        assert!(d.lock().get("a").is_none());
    }

    #[test]
    fn an_answer_for_an_upstream_that_changed_meanwhile_is_dropped() {
        let d = Directory::default();
        let mut c = cfg(vec![provider("a")]);
        d.reconcile(&c);
        let before = identity(&c, &c.providers[0]);
        c.providers[0].base_url = "https://elsewhere.example".into();
        d.reconcile(&c);
        d.record("a", &before, Answer::Listed(vec!["stale".into()]), 1);
        assert_eq!(d.listing(&c.providers[0]).source, Source::None);
    }

    #[test]
    fn the_catalog_leaves_out_disabled_upstreams_and_models_out_of_scope() {
        let d = Directory::default();
        let mut c = cfg(vec![provider("a"), provider("b"), provider("c")]);
        d.reconcile(&c);
        for p in &c.providers {
            d.record(
                &p.name,
                &identity(&c, p),
                Answer::Listed(vec![format!("{}-1", p.name), format!("{}-2", p.name)]),
                1,
            );
        }
        c.providers[1].disabled = true;
        c.providers[2].models_only = Some(vec!["c-1".into()]);
        let cat = published(&d, &c);
        assert_eq!(cat.all(), ["a-1", "a-2", "c-1"]);
        let s = serving(&c, &cat, &["a".into(), "b".into(), "c".into()], "c-2");
        assert_eq!(
            s.skipped,
            [
                ("a".to_string(), Skip::NotOffered),
                ("b".to_string(), Skip::Disabled),
                ("c".to_string(), Skip::OutOfScope)
            ]
        );
        assert!(s.usable.is_empty());
    }

    #[test]
    fn an_upstream_without_a_list_is_kept_and_a_failure_retries_sooner_than_a_day() {
        let d = Directory::default();
        let c = cfg(vec![provider("a"), provider("b")]);
        d.reconcile(&c);
        d.record(
            "a",
            &identity(&c, &c.providers[0]),
            Answer::Failed("连不上".into()),
            0,
        );
        d.record(
            "b",
            &identity(&c, &c.providers[1]),
            Answer::NoList("没有接口".into()),
            0,
        );
        let cat = published(&d, &c);
        // 不知道它们有什么：不跳过
        let s = serving(&c, &cat, &["a".into(), "b".into()], "m");
        assert_eq!(s.usable, ["a", "b"]);
        // 失败的一小时后重问，没有接口的一天后
        let hour = RETRY_AFTER.as_millis() as u64;
        assert_eq!(d.due(&c, hour), ["a"]);
        assert_eq!(d.due(&c, REFRESH_EVERY.as_millis() as u64), ["a", "b"]);
        assert_eq!(
            d.listing(&c.providers[1]).error.as_deref(),
            Some("没有接口")
        );
    }
}
