//! 一段对话留在上次回答它的那一家；一轮开始时的路由决定沿用到这一轮结束。
//!
//! **缓存按上游隔离。**客户端每个请求都带着整段对话，上游把读过的开头缓存起来，下
//! 一个请求读缓存只付一成的价钱。换一家，那一家要全价重算整段上下文 —— 一段长对话
//! 半路换一次家，就是几十万 token 的全价输入。所以：
//!
//! - **同一轮之内一律不换。**客户端在回传工具结果（agent 还在干这一轮的活）时，路由
//!   决定不重新求值，上游也留在上次回答的那一家。按输入大小、有没有图片分流的规则，
//!   在一轮里随着上下文变长会翻过去 —— 以前每个请求都重新求值，一轮半路就换了家。
//!   只有输入超出了所选模型的上下文时才重新求值：那时不换也发不出去。
//! - **跨轮时看缓存值不值得留**：上一次回答读写了至少 [`CACHE_WORTH`] 个 token 的缓存、
//!   离现在不到 [`CACHE_COLD_MS`]（各家缓存最短的存活时间）才留下。缓存没有或已经
//!   凉了，留下什么都省不到，策略组照常排序（`load-balance` 就是在这时轮到下一家）。
//! - **留在「实际回答的那一家」**，不是路由排的头一个：故障转移之后接下回答的备选，
//!   就是之后留下的那一家 —— 缓存在它那儿。
//! - **那一家不可用（熔断着、冷却中）就放开**，照常排序：留在一家发不出去的上游
//!   上，省下的缓存一分钱都兑现不了。
//!
//! 对话按「路由 + 对话标识」记（见 [`Conversation`]），回答的那一家还要对得上策略组：
//! 同一段对话这一轮被规则送去另一个组，上次那一家的缓存和这一组无关。
//!
//! **只在内存里**，最多记 [`CAP`] 段、每段最多 [`KEEP_MS`]：core 重启之后第一轮
//! 按策略重新排，最坏是一次缓存重建。**跨配置重载存活**，但沿用的路由决定只在同一
//! 份配置里有效（配置改了，规则可能已经不是那条了）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use axum::http::HeaderMap;
use tw_dialect::ir;
use tw_engine::{Decision, Engine};

/// 上一次回答读写了这么多 token 的缓存，跨轮才值得留下。
///
/// Anthropic 和 OpenAI 最短能缓存的前缀都是 1024 个 token：少于它，上游本来就不缓存。
/// **读和写都算**：头一轮往往只有写（上游刚缓存下来），下一轮正是要读它的时候。
pub const CACHE_WORTH: u64 = 1024;

/// 上一次回答离现在超过这么久，缓存就当凉了。Anthropic 和 OpenAI 不被读取的缓存
/// 最短都只存五分钟
pub const CACHE_COLD_MS: u64 = 5 * 60 * 1000;

/// 一段对话记多久。一天没动静的对话，缓存早就没了，沿用的路由决定也不再有意义
pub const KEEP_MS: u64 = 24 * 60 * 60 * 1000;

/// 最多记几段对话。满了先清掉超期的，还满就清最久没动静的那段
pub const CAP: usize = 4096;

/// 客户端自己说出对话身份的请求头：Claude Code、Codex、OpenCode 各有各的写法。
const SESSION_HEADERS: &[&str] = &[
    "x-claude-code-session-id",
    "session_id",
    "session-id",
    "conversation_id",
    "x-session-affinity",
    "x-opencode-session",
];

/// 这次请求属于哪段对话。
///
/// **客户端带着会话头时用它，再加上按内容认的指纹**（[`crate::session::fingerprint`]）。
/// 只用会话头不够：Claude Code 的一个会话里还有子代理和起标题这类旁支请求，会话头
/// 一样，系统提示和首条消息不一样，缓存也各是各的 —— 并成一段的话，旁支请求一来就
/// 把正在干活的那一轮的记录冲掉。只用指纹也不够：同一个仓库里开头一字不差的两段
/// 对话会被并成一段。
///
/// 两样都没有就是 `None`：认不出来的对话不记，每个请求照常排序。
pub fn identity(headers: &HeaderMap, fp: Option<&str>) -> Option<String> {
    let named = SESSION_HEADERS.iter().find_map(|h| {
        headers
            .get(*h)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    });
    match (named, fp) {
        // 会话头是客户端给的原文，不进记录：只留一个指纹
        (Some(named), fp) => {
            let h = blake3::hash(format!("{named}\n{}", fp.unwrap_or_default()).as_bytes());
            Some(format!("s-{}", &h.to_hex()[..24]))
        }
        (None, Some(fp)) => Some(fp.to_string()),
        (None, None) => None,
    }
}

/// 对话走到了第几轮。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Turn {
    /// 用户说过几次话：带着文字、图片或文件，而不是工具结果的 user 消息
    pub number: u32,
    /// 最后一条 user 消息是工具结果：agent 还在这一轮里干活
    pub within: bool,
}

/// 从请求里的整段对话数出这是第几轮。
///
/// **看的是最后一条 user 消息**：agent 可以在工具结果后面附上自己的话（Claude Code
/// 会在同一条消息里跟一段 system-reminder 文字），那还是同一轮 —— 所以一条消息里
/// 有工具结果，就不算用户又说了一次话。
pub fn turn_of(r: &ir::Request) -> Turn {
    let mut t = Turn::default();
    for m in r.messages.iter().filter(|m| m.role == ir::Role::User) {
        let result = m.parts.iter().any(|p| matches!(p, ir::Part::ToolResult(_)));
        let said = m.parts.iter().any(|p| {
            matches!(
                p,
                ir::Part::Text(_) | ir::Part::Image(_) | ir::Part::File { .. }
            )
        });
        if said && !result {
            t.number += 1;
        }
        t.within = result;
    }
    t
}

/// 一段对话在一条路由里的这一轮。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    /// 路由名 + 对话标识：不同路由（不同的密钥）里的同一段对话各记各的
    key: String,
    pub turn: Turn,
}

impl Conversation {
    pub fn new(route: &str, id: &str, turn: Turn) -> Self {
        Self {
            key: format!("{route}\n{id}"),
            turn,
        }
    }
}

/// 一段对话记着的。
struct Entry {
    /// 最后一次动它的时刻：超期和清理都按它
    touched_ms: u64,
    /// 这一轮开头定下的路由决定
    held: Option<Held>,
    /// 上次回答它的那一家
    answer: Option<Answer>,
}

struct Held {
    turn: u32,
    /// 是哪一份配置定的。**配置换了就不沿用**：规则可能已经改了、删了。存弱引用：
    /// 旧引擎的那块内存在它还被引用时不会被复用，比较的不会是另一份配置
    engine: Weak<Engine>,
    decision: Decision,
}

struct Answer {
    /// 回答时规则把它交给的策略组。直接指上游的是 None
    group: Option<String>,
    provider: String,
    turn: u32,
    at_ms: u64,
    /// 那次回答读写的缓存 token
    cache: u64,
}

/// 每段对话的这一轮路由决定、上次回答它的那一家。
#[derive(Default)]
pub struct Affinity {
    entries: Mutex<HashMap<String, Entry>>,
}

impl Affinity {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        // 锁中毒了照样拿里面的表：少留一次缓存，好过一个不转发的网关
        self.entries.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 这一轮开头定下的路由决定。**只在同一轮里、同一份配置下**才有。
    pub fn held(&self, c: &Conversation, engine: &Arc<Engine>, now_ms: u64) -> Option<Decision> {
        if !c.turn.within {
            return None;
        }
        let map = self.lock();
        let e = map.get(&c.key).filter(|e| fresh(e, now_ms))?;
        let h = e.held.as_ref()?;
        (h.turn == c.turn.number && Weak::ptr_eq(&h.engine, &Arc::downgrade(engine)))
            .then(|| h.decision.clone())
    }

    /// 记下这一轮的路由决定：规则刚求过值的那一个。
    pub fn decided(&self, c: &Conversation, engine: &Arc<Engine>, d: &Decision, now_ms: u64) {
        let held = Held {
            turn: c.turn.number,
            engine: Arc::downgrade(engine),
            decision: d.clone(),
        };
        self.upsert(&c.key, now_ms, |e| e.held = Some(held));
    }

    /// 该不该留在上次回答这段对话的那一家。该留就把它挪到 `candidates` 的头上，
    /// 交回留下的理由。
    ///
    /// `group` 是这次规则交给的策略组；`available` 说一家现在能不能进候选链（熔断、
    /// 冷却）。只剩一个候选时没有「留不留」可言。
    pub fn stay(
        &self,
        c: &Conversation,
        group: Option<&str>,
        candidates: &mut Vec<String>,
        available: impl Fn(&str) -> bool,
        now_ms: u64,
    ) -> Option<tw_api::Stay> {
        if candidates.len() < 2 {
            return None;
        }
        let map = self.lock();
        let a = map
            .get(&c.key)
            .filter(|e| fresh(e, now_ms))?
            .answer
            .as_ref()?;
        if a.group.as_deref() != group {
            return None;
        }
        let at = candidates.iter().position(|p| *p == a.provider)?;
        if !available(&a.provider) {
            return None;
        }
        let why = if c.turn.within && a.turn == c.turn.number {
            tw_api::Stay::Turn
        } else if a.cache >= CACHE_WORTH && now_ms.saturating_sub(a.at_ms) < CACHE_COLD_MS {
            tw_api::Stay::Cache
        } else {
            return None;
        };
        let p = candidates.remove(at);
        candidates.insert(0, p);
        Some(why)
    }

    /// 这段对话这一次由谁回答，交给请求的结局：成功了才记（见 [`Ticket::answered`]）。
    pub fn ticket(
        self: &Arc<Self>,
        c: &Conversation,
        group: Option<String>,
        provider: &str,
    ) -> Ticket {
        Ticket {
            store: Arc::clone(self),
            key: c.key.clone(),
            group,
            provider: provider.to_string(),
            turn: c.turn.number,
        }
    }

    fn answered(&self, t: &Ticket, cache: u64, now_ms: u64) {
        let answer = Answer {
            group: t.group.clone(),
            provider: t.provider.clone(),
            turn: t.turn,
            at_ms: now_ms,
            cache,
        };
        self.upsert(&t.key, now_ms, |e| e.answer = Some(answer));
    }

    /// `provider` 在这段对话上没了声音（无响应超时，见 `server::pipeline::idle`）：上次回答它
    /// 的要是这一家，就不再记着 —— 这一轮接下来的请求照常排序，不会因为「上次回答的就是它」
    /// 又被送回去。这一轮的路由决定照旧沿用
    pub fn left(&self, c: &Conversation, provider: &str) {
        self.forget(&c.key, provider);
    }

    fn forget(&self, key: &str, provider: &str) {
        let mut map = self.lock();
        if let Some(e) = map.get_mut(key)
            && e.answer.as_ref().is_some_and(|a| a.provider == provider)
        {
            e.answer = None;
        }
    }

    fn upsert(&self, key: &str, now_ms: u64, f: impl FnOnce(&mut Entry)) {
        let mut map = self.lock();
        if !map.contains_key(key) && map.len() >= CAP {
            map.retain(|_, e| fresh(e, now_ms));
            if map.len() >= CAP
                && let Some(oldest) = map
                    .iter()
                    .min_by_key(|(_, e)| e.touched_ms)
                    .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        let e = map.entry(key.to_string()).or_insert(Entry {
            touched_ms: now_ms,
            held: None,
            answer: None,
        });
        // 超期的整段作废：一天前那一轮的决定和回答，对现在的请求都没有意义
        if !fresh(e, now_ms) {
            e.held = None;
            e.answer = None;
        }
        e.touched_ms = e.touched_ms.max(now_ms);
        f(e);
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.lock().len()
    }
}

fn fresh(e: &Entry, now_ms: u64) -> bool {
    now_ms.saturating_sub(e.touched_ms) <= KEEP_MS
}

/// 这一次由谁回答。**跟着请求的结局走**：上游回了成功、响应走完了才记下它和它读写
/// 了多少缓存（见 [`crate::ending::Ending`]）。
pub struct Ticket {
    store: Arc<Affinity>,
    key: String,
    group: Option<String>,
    provider: String,
    turn: u32,
}

impl Ticket {
    /// 回答完了：那一家读写了 `cache` 个 token 的缓存。
    pub fn answered(self, cache: u64) {
        self.store.answered(&self, cache, crate::server::now_ms());
    }

    /// 这一家答到一半没了声音：上次回答这段对话的要是它，不再记着（见 [`Affinity::left`]）
    pub fn left(self) {
        self.store.forget(&self.key, &self.provider);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tw_dialect::ir::{Message, Part, Role, ToolResult};

    const MIN: u64 = 60_000;

    fn conv(number: u32, within: bool) -> Conversation {
        Conversation::new("default", "对话", Turn { number, within })
    }

    /// 没了声音的那一家：这一轮不再留在它那儿，别家的回答不受影响
    #[test]
    fn a_provider_that_went_quiet_is_not_stayed_on() {
        let a = Arc::new(Affinity::default());
        let c = conv(1, true);
        a.ticket(&c, None, "甲").answered(5000);
        let stay = |a: &Affinity| {
            let mut cands = vec!["乙".to_string(), "甲".to_string()];
            a.stay(&c, None, &mut cands, |_| true, crate::server::now_ms())
                .map(|_| cands[0].clone())
        };
        assert_eq!(stay(&a).as_deref(), Some("甲"));
        a.left(&c, "乙");
        assert_eq!(stay(&a).as_deref(), Some("甲"), "说的不是它，照旧");
        a.left(&c, "甲");
        assert_eq!(stay(&a), None);
        // 通过回答的票据放开也一样
        a.ticket(&c, None, "甲").answered(5000);
        a.ticket(&c, None, "甲").left();
        assert_eq!(stay(&a), None);
    }

    fn engine() -> Arc<Engine> {
        Arc::new(Engine::with_default_rules(
            vec!["甲".into(), "乙".into()],
            vec![],
            vec![],
        ))
    }

    fn decision(rule: &str) -> Decision {
        Decision {
            candidates: vec!["甲".into(), "乙".into()],
            matched_rule: rule.into(),
            via_group: Some("池子".into()),
            set: Default::default(),
            rewritten_by: vec![],
            pinned: vec![],
        }
    }

    fn pool() -> Vec<String> {
        vec!["甲".into(), "乙".into(), "丙".into()]
    }

    fn answer(a: &Arc<Affinity>, c: &Conversation, provider: &str, cache: u64, at: u64) {
        let t = a.ticket(c, Some("池子".into()), provider);
        a.answered(&t, cache, at);
    }

    fn up(_: &str) -> bool {
        true
    }

    #[test]
    fn a_session_header_names_the_conversation_and_the_content_tells_its_branches_apart() {
        let mut h = HeaderMap::new();
        h.insert("x-claude-code-session-id", "sess-1".parse().unwrap());
        let main = identity(&h, Some("主对话")).unwrap();
        // 同一个会话头、同一段内容：同一段对话
        assert_eq!(identity(&h, Some("主对话")), Some(main.clone()));
        // 同一个会话头下的旁支请求（子代理、起标题）：内容不一样，各是各的
        assert_ne!(identity(&h, Some("子代理")), Some(main.clone()));
        // 内容一字不差的两段对话，靠会话头分开
        let mut other = HeaderMap::new();
        other.insert("x-claude-code-session-id", "sess-2".parse().unwrap());
        assert_ne!(identity(&other, Some("主对话")), Some(main.clone()));
        // Codex 的写法也认
        let mut codex = HeaderMap::new();
        codex.insert("session_id", "sess-1".parse().unwrap());
        assert!(identity(&codex, Some("主对话")).is_some());
        // 会话头原文不进标识
        assert!(!main.contains("sess-1"), "{main}");
    }

    #[test]
    fn without_a_header_the_content_fingerprint_is_the_conversation() {
        let h = HeaderMap::new();
        assert_eq!(identity(&h, Some("fp-1")), Some("fp-1".into()));
        // 空的会话头不算数
        let mut blank = HeaderMap::new();
        blank.insert("session_id", "  ".parse().unwrap());
        assert_eq!(identity(&blank, Some("fp-1")), Some("fp-1".into()));
        // 什么都认不出来就不记
        assert_eq!(identity(&h, None), None);
    }

    fn user(parts: Vec<Part>) -> Message {
        Message {
            role: Role::User,
            parts,
        }
    }

    fn result() -> Part {
        Part::ToolResult(ToolResult {
            id: "t".into(),
            content: vec![],
            is_error: false,
        })
    }

    #[test]
    fn tool_results_continue_the_turn_and_a_new_user_message_starts_one() {
        let text = |s: &str| Part::Text(s.into());
        let mut r = ir::Request {
            messages: vec![user(vec![text("改一下")])],
            ..Default::default()
        };
        assert_eq!(
            turn_of(&r),
            Turn {
                number: 1,
                within: false
            }
        );
        // 工具结果回来了，后面跟着 agent 自己的一段话：还是第一轮
        r.messages.push(Message {
            role: Role::Assistant,
            parts: vec![],
        });
        r.messages
            .push(user(vec![result(), text("<system-reminder>")]));
        assert_eq!(
            turn_of(&r),
            Turn {
                number: 1,
                within: true
            }
        );
        // 用户又说话了：第二轮
        r.messages.push(user(vec![text("再改一下")]));
        assert_eq!(
            turn_of(&r),
            Turn {
                number: 2,
                within: false
            }
        );
    }

    #[test]
    fn within_a_turn_the_route_decided_at_its_start_is_kept() {
        let a = Arc::new(Affinity::default());
        let e = engine();
        // 这一轮开头：规则求值，记下
        a.decided(&conv(3, false), &e, &decision("小输入"), 0);
        // 同一轮里的下一个请求：沿用，即使规则现在会给出别的
        assert_eq!(
            a.held(&conv(3, true), &e, MIN).map(|d| d.matched_rule),
            Some("小输入".into())
        );
        // 新的一轮重新求值
        assert_eq!(a.held(&conv(4, false), &e, MIN), None);
        // 轮数对不上（记着的是别的轮）也不沿用
        assert_eq!(a.held(&conv(4, true), &e, MIN), None);
        // 配置换了：规则可能已经不是那条了
        assert_eq!(a.held(&conv(3, true), &engine(), MIN), None);
        // 另一条路由里的同一段对话各记各的
        let elsewhere = Conversation::new("另一条", "对话", conv(3, true).turn);
        assert_eq!(a.held(&elsewhere, &e, MIN), None);
    }

    #[test]
    fn within_a_turn_the_one_that_answered_stays_first_whatever_its_cache() {
        let a = Arc::new(Affinity::default());
        answer(&a, &conv(2, false), "乙", 0, 0);
        let mut c = pool();
        assert_eq!(
            a.stay(&conv(2, true), Some("池子"), &mut c, up, 30 * MIN),
            Some(tw_api::Stay::Turn),
            "同一轮里不看缓存、也不看隔了多久"
        );
        assert_eq!(c, ["乙", "甲", "丙"], "其余的顺次留着做故障转移");
    }

    #[test]
    fn across_turns_it_stays_only_while_the_cache_is_worth_it_and_warm() {
        let a = Arc::new(Affinity::default());
        let next = conv(3, false);
        // 缓存够、没凉：留
        answer(&a, &conv(2, true), "丙", CACHE_WORTH, 0);
        let mut c = pool();
        assert_eq!(
            a.stay(&next, Some("池子"), &mut c, up, CACHE_COLD_MS - 1),
            Some(tw_api::Stay::Cache)
        );
        assert_eq!(c[0], "丙");
        // 凉了：放开，顺序照策略
        let mut c = pool();
        assert_eq!(a.stay(&next, Some("池子"), &mut c, up, CACHE_COLD_MS), None);
        assert_eq!(c, pool());
        // 缓存太少：没什么可留的
        answer(&a, &conv(2, true), "丙", CACHE_WORTH - 1, 0);
        let mut c = pool();
        assert_eq!(a.stay(&next, Some("池子"), &mut c, up, MIN), None);
        assert_eq!(c, pool());
    }

    #[test]
    fn a_pinned_upstream_that_is_cooling_down_is_released() {
        let a = Arc::new(Affinity::default());
        answer(&a, &conv(1, false), "乙", 50_000, 0);
        let mut c = pool();
        let open = |p: &str| p != "乙";
        assert_eq!(
            a.stay(&conv(1, true), Some("池子"), &mut c, open, MIN),
            None
        );
        assert_eq!(c, pool(), "熔断着的那家不该被挪到头上");
    }

    #[test]
    fn the_failover_member_that_answered_becomes_the_one_it_stays_on() {
        let a = Arc::new(Affinity::default());
        // 路由排的头一个是甲，甲失败、乙接下了回答：记的是乙
        answer(&a, &conv(1, false), "乙", 8_000, 0);
        let mut c = pool();
        assert_eq!(
            a.stay(&conv(1, true), Some("池子"), &mut c, up, MIN),
            Some(tw_api::Stay::Turn)
        );
        assert_eq!(c[0], "乙");
        // 下一次又转移到了丙：之后留在丙
        answer(&a, &conv(1, true), "丙", 8_000, 2 * MIN);
        let mut c = pool();
        a.stay(&conv(1, true), Some("池子"), &mut c, up, 3 * MIN);
        assert_eq!(c[0], "丙");
    }

    #[test]
    fn another_group_or_a_member_no_longer_offered_does_not_pin() {
        let a = Arc::new(Affinity::default());
        answer(&a, &conv(1, false), "乙", 8_000, 0);
        // 这一轮规则把它交给了另一个组
        let mut c = pool();
        assert_eq!(
            a.stay(&conv(1, true), Some("别的组"), &mut c, up, MIN),
            None
        );
        // 那一家不在这次的候选里（停用了、没有这个模型）
        let mut c: Vec<String> = vec!["甲".into(), "丙".into()];
        assert_eq!(a.stay(&conv(1, true), Some("池子"), &mut c, up, MIN), None);
        // 只有一个候选：没有留不留可言
        let mut c: Vec<String> = vec!["乙".into()];
        assert_eq!(a.stay(&conv(1, true), Some("池子"), &mut c, up, MIN), None);
    }

    #[test]
    fn a_day_old_conversation_is_forgotten_and_the_table_stays_bounded() {
        let a = Arc::new(Affinity::default());
        answer(&a, &conv(1, false), "乙", 8_000, 0);
        let mut c = pool();
        assert_eq!(
            a.stay(&conv(1, true), Some("池子"), &mut c, up, KEEP_MS + 1),
            None
        );
        for i in 0..CAP as u64 {
            let c = Conversation::new("default", &format!("旧-{i}"), Turn::default());
            answer(&a, &c, "甲", 0, i);
        }
        assert_eq!(a.tracked(), CAP);
        // 满了：清掉最久没动静的那段，新的照样记得住
        answer(&a, &conv(9, false), "丙", 0, CAP as u64);
        assert_eq!(a.tracked(), CAP);
        let mut c = pool();
        assert_eq!(
            a.stay(&conv(9, true), Some("池子"), &mut c, up, CAP as u64 + 1),
            Some(tw_api::Stay::Turn)
        );
        // 超期的一次清光
        answer(&a, &conv(10, false), "丙", 0, KEEP_MS + CAP as u64 + 10);
        let fresh_key = Conversation::new("default", "新的", Turn::default());
        answer(&a, &fresh_key, "丙", 0, 2 * KEEP_MS + CAP as u64 + 10);
        assert!(a.tracked() <= 2, "{}", a.tracked());
    }
}
