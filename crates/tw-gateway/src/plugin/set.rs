//! 跟着配置一起换的那一份插件，和跨重载存活的计数与日志。
//!
//! **一次换入就是一整份**（见 `Runtime`）：一个请求从头到尾看到的是同一份插件 ——
//! 请求钩子和回答钩子之间配置换了，这个请求照样按它开始时的那一份走完。
//!
//! 计数和日志**不跟着换**：改一个设置、批准一次文件不该把「从启动以来跑了多少次」
//! 清零。它们按插件 id 挂在一张跨重载的表上，每份插件拿到的是同一个 `Arc`。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tw_api::{OnError, PluginHook, PluginOutcome, ReplyMode};
use tw_types::Msg;

use crate::plugin::engine::{Hooks, Manifest};
use crate::plugin::host::PluginHost;

/// 一个插件管哪些请求。**每张单子里都是 `*` 通配**（不分大小写，和路由规则同一种），
/// 空着是「都管」。
///
/// **按每一次发往上游来看**（契约附录二）：请求钩子排在路由之后，每试一家上游跑一次，
/// 那时这一次的客户端、发出去的模型和上游都定了 —— 请求钩子和回答钩子看的是同三样。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    /// 客户端应用：`claude-code`、`codex`……（请求记录上的 `client_hint`）
    pub clients: Vec<String>,
    /// **发给上游的模型**：路由规则改写过的是改写之后的那个，不是客户端写的
    pub models: Vec<String>,
    /// 这一次发往的上游
    pub upstreams: Vec<String>,
}

impl Scope {
    /// 管不管发往 `upstream`、模型名是 `model` 的这一次。认不出是哪个应用（`client`
    /// 是 None）时，只有不挑应用的插件管它
    pub fn covers(&self, client: Option<&str>, model: &str, upstream: &str) -> bool {
        listed(&self.clients, client)
            && listed(&self.models, Some(model))
            && listed(&self.upstreams, Some(upstream))
    }
}

fn listed(patterns: &[String], value: Option<&str>) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let Some(v) = value else { return false };
    patterns
        .iter()
        .any(|p| tw_engine::rule::glob_match(p.trim(), v))
}

/// 一个插件此刻能不能跑。
#[derive(Clone)]
pub enum State {
    /// 编好了，哈希和批准的一致
    Ready(Arc<dyn PluginHost>),
    /// 跑不了。**它管的请求照它的 `on_error` 处置**：拒绝，或者跳过它
    Broken(Broken),
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            State::Ready(h) => f.debug_tuple("Ready").field(&h.manifest().name).finish(),
            State::Broken(b) => f.debug_tuple("Broken").field(b).finish(),
        }
    }
}

/// 跑不了的原因。
#[derive(Debug, Clone, PartialEq)]
pub enum Broken {
    /// 磁盘上的文件和批准过的那一份不一样了（或者没了）。**改过的代码不跑**，
    /// 要在应用里看过改动、重新批准
    Changed,
    /// 加载不了：语法错、manifest 不合规矩、设置和 manifest 对不上、读不了文件……
    Error(Msg),
}

/// 配置里的一个插件，此刻的样子。
#[derive(Debug)]
pub struct Active {
    pub id: String,
    /// 插件自己起的名字（manifest 的 `name`）。读不出 manifest 时是 id。**插件写的字**
    pub name: String,
    pub enabled: bool,
    /// 出错时怎么办：插件文件里写的（manifest 的 `on_error`）。读不出 manifest 时照批准的那份
    /// 字节里写着的，再读不出是拒绝（见 [`crate::plugin::load`]）
    pub on_error: OnError,
    /// 管哪些请求：插件文件里写的（manifest 的 `match`），读不出时同 `on_error`
    pub scope: Scope,
    /// 读不出 manifest 时是空的
    pub permissions: Vec<tw_api::Permission>,
    /// 处理哪几种请求（manifest 的 `requests`）。**读不出 manifest 时按出厂的算**（只有
    /// 对话，[`crate::plugin::engine::DEFAULT_REQUESTS`]）：说不出它声明过什么，就按不写
    /// `requests` 的插件对待 —— 它拦的是对话，嵌入、补全照常过去
    pub requests: Vec<tw_api::RequestKind>,
    pub reply_mode: ReplyMode,
    pub hooks: Hooks,
    /// 交给插件的设置：manifest 里每个设置此刻的值（`value`），加载时核对过类型。读不出
    /// manifest 时是空的
    pub settings: serde_json::Map<String, serde_json::Value>,
    /// 读出来的 manifest。文件变了时是批准过的那一份的（只拿来显示，不跑）；
    /// 哪一份都读不出来时是 None
    pub manifest: Option<Manifest>,
    pub state: State,
    pub stats: Arc<Stats>,
    pub logs: Arc<LogRing>,
}

impl Active {
    pub fn ready(&self) -> Option<&Arc<dyn PluginHost>> {
        match &self.state {
            State::Ready(h) => Some(h),
            State::Broken(_) => None,
        }
    }

    pub fn broken(&self) -> Option<&Broken> {
        match &self.state {
            State::Ready(_) => None,
            State::Broken(b) => Some(b),
        }
    }

    /// 处不处理这一种请求。**不处理的种类在它的范围之外**：那种请求不过它，它跑不了、
    /// 出了错也拦不着那种请求
    pub fn handles(&self, kind: tw_api::RequestKind) -> bool {
        self.requests.contains(&kind)
    }
}

/// 一份插件，按配置里的顺序 —— **也就是运行的顺序**。
#[derive(Debug, Default)]
pub struct PluginSet {
    plugins: Vec<Arc<Active>>,
}

impl PluginSet {
    pub fn new(plugins: Vec<Arc<Active>>) -> Self {
        Self { plugins }
    }

    /// 配置里的全部，停用的、跑不了的也在
    pub fn all(&self) -> &[Arc<Active>] {
        &self.plugins
    }

    pub fn get(&self, id: &str) -> Option<&Arc<Active>> {
        self.plugins.iter().find(|p| p.id == id)
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// 发往一个上游之前要过一遍的插件，按顺序：启用的、处理 `kind` 这种请求的、管得着
    /// 这一次的，**连同跑不了的** —— 跑不了的由调用方照它的 `on_error` 拒绝请求或者跳过它
    /// （管得着就要处置，不管它有没有请求钩子：它一旦加载不了，回答那一段同样做不了）。
    /// 能跑的只列有请求钩子的。**管不着这一次的不算**：只管别的上游的插件坏了，拦不着发往
    /// 这一家的请求；只处理对话的插件坏了，拦不着嵌入和补全。
    pub fn for_request(
        &self,
        kind: tw_api::RequestKind,
        client: Option<&str>,
        model: &str,
        upstream: &str,
    ) -> Vec<Arc<Active>> {
        self.plugins
            .iter()
            .filter(|p| p.enabled && p.handles(kind) && p.scope.covers(client, model, upstream))
            .filter(|p| p.ready().is_none() || p.hooks.request)
            .cloned()
            .collect()
    }

    /// 这个回答上要过一遍的插件，按顺序：启用的、能跑的、有回答钩子的、管得着回答它的
    /// 那一次的。**跑不了的不在这里**：它们在那一次发出去之前已经处置过了。回答钩子只在
    /// 对话上跑：不处理对话的插件不在这里（它也不该有回答钩子，清单校验时就拦了）
    pub fn for_reply(&self, client: Option<&str>, model: &str, upstream: &str) -> Vec<Arc<Active>> {
        self.plugins
            .iter()
            .filter(|p| p.enabled && p.ready().is_some() && p.hooks.on_reply())
            .filter(|p| p.handles(tw_api::RequestKind::Conversation))
            .filter(|p| p.scope.covers(client, model, upstream))
            .cloned()
            .collect()
    }
}

/// 插件写的一行日志（`console.log` 这一类）。
#[derive(Debug, Clone, PartialEq)]
pub struct LogLine {
    pub level: tw_api::PluginLogLevel,
    pub text: String,
}

/// 一个插件在一个请求上的一次运行。**落库的就是它**（`plugin_runs` 一行）。
///
/// 请求钩子一次一条；回答钩子一个回答一条，改了几段文字、几个工具调用记在 `detail` 里。
#[derive(Debug, Clone, PartialEq)]
pub struct PluginRun {
    pub plugin_id: String,
    /// 当时的名字：插件之后改了名，这条记录说的还是当时那个
    pub plugin_name: String,
    pub hook: PluginHook,
    pub outcome: PluginOutcome,
    /// 出错、拒绝时的原因
    pub error: Option<Msg>,
    /// 用了多少 CPU，微秒。回答钩子是这个回答上各次调用加起来
    pub cpu_us: u64,
    /// 细节，JSON（回答钩子改了几处之类）
    pub detail: Option<serde_json::Value>,
}

/// 一个插件从这次启动以来的计数。**在内存里、跨重载存活**。
#[derive(Debug, Default)]
pub struct Stats {
    calls: AtomicU64,
    changed: AtomicU64,
    rejected: AtomicU64,
    errors: AtomicU64,
    cpu_us: AtomicU64,
    last_error: Mutex<Option<tw_api::PluginLastError>>,
}

impl Stats {
    /// 记一次运行。**跳过的不算一次调用**：插件根本没跑
    pub fn note(&self, run: &PluginRun, at_ms: u64) {
        if run.outcome == PluginOutcome::Skipped {
            return;
        }
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.cpu_us.fetch_add(run.cpu_us, Ordering::Relaxed);
        match run.outcome {
            PluginOutcome::Changed => {
                self.changed.fetch_add(1, Ordering::Relaxed);
            }
            PluginOutcome::Rejected => {
                self.rejected.fetch_add(1, Ordering::Relaxed);
            }
            PluginOutcome::Error => {
                self.errors.fetch_add(1, Ordering::Relaxed);
                if let Some(message) = run.error.clone() {
                    *self
                        .last_error
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) =
                        Some(tw_api::PluginLastError { at_ms, message });
                }
            }
            PluginOutcome::Unchanged | PluginOutcome::Skipped => {}
        }
    }

    pub fn view(&self) -> tw_api::PluginStats {
        let calls = self.calls.load(Ordering::Relaxed);
        tw_api::PluginStats {
            calls,
            changed: self.changed.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            avg_cpu_us: self
                .cpu_us
                .load(Ordering::Relaxed)
                .checked_div(calls)
                .unwrap_or(0),
            last_error: self
                .last_error
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        }
    }
}

/// 一个插件最近的日志，最多 [`LogRing::CAP`] 行，满了丢最老的。**只在内存里**。
#[derive(Debug, Default)]
pub struct LogRing {
    lines: Mutex<VecDeque<tw_api::PluginLogEntry>>,
}

impl LogRing {
    pub const CAP: usize = 500;

    /// 记下一次运行写的几行
    pub fn extend(
        &self,
        at_ms: u64,
        request_id: Option<u64>,
        hook: PluginHook,
        lines: Vec<LogLine>,
    ) {
        if lines.is_empty() {
            return;
        }
        let mut g = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        for l in lines {
            if g.len() >= Self::CAP {
                g.pop_front();
            }
            g.push_back(tw_api::PluginLogEntry {
                at_ms,
                request_id,
                hook,
                level: l.level,
                text: l.text,
            });
        }
    }

    /// 全部，老的在前
    pub fn lines(&self) -> Vec<tw_api::PluginLogEntry> {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(clients: &[&str], models: &[&str], upstreams: &[&str]) -> Scope {
        let v = |x: &[&str]| x.iter().map(|s| s.to_string()).collect();
        Scope {
            clients: v(clients),
            models: v(models),
            upstreams: v(upstreams),
        }
    }

    struct Host(Manifest);

    impl PluginHost for Host {
        fn manifest(&self) -> &Manifest {
            &self.0
        }
        fn sha256(&self) -> [u8; 32] {
            [0; 32]
        }
    }

    fn active(id: &str, hooks: Hooks, scope: Scope, state: Option<Broken>) -> Arc<Active> {
        let manifest = Manifest {
            name: id.to_string(),
            api: 1,
            description: None,
            permissions: Vec::new(),
            requests: crate::plugin::engine::DEFAULT_REQUESTS.to_vec(),
            scope: Scope::default(),
            on_error: OnError::Reject,
            reply_mode: ReplyMode::Block,
            settings: Vec::new(),
            hooks,
        };
        let state = match state {
            Some(b) => State::Broken(b),
            None => State::Ready(Arc::new(Host(manifest.clone()))),
        };
        Arc::new(Active {
            id: id.to_string(),
            name: id.to_string(),
            enabled: true,
            on_error: OnError::Reject,
            scope,
            permissions: Vec::new(),
            requests: crate::plugin::engine::DEFAULT_REQUESTS.to_vec(),
            reply_mode: ReplyMode::Block,
            hooks,
            settings: Default::default(),
            manifest: Some(manifest),
            state,
            stats: Default::default(),
            logs: Default::default(),
        })
    }

    const REQUEST: Hooks = Hooks {
        request: true,
        reply_text: false,
        reply_text_end: false,
        tool_call: false,
    };
    const REPLY: Hooks = Hooks {
        request: false,
        reply_text: true,
        reply_text_end: false,
        tool_call: false,
    };

    const CONVERSATION: tw_api::RequestKind = tw_api::RequestKind::Conversation;

    fn ids(v: &[Arc<Active>]) -> Vec<&str> {
        v.iter().map(|p| p.id.as_str()).collect()
    }

    #[test]
    fn an_empty_list_covers_everything_and_globs_ignore_case() {
        let all = Scope::default();
        assert!(all.covers(None, "anything", "anywhere"));
        assert!(all.covers(Some("codex"), "gpt-5", "openai"));

        let s = scope(&["claude-*"], &["Claude-Sonnet-*"], &["anthropic"]);
        assert!(s.covers(Some("claude-code"), "claude-sonnet-4-5", "anthropic"));
        assert!(!s.covers(Some("codex"), "claude-sonnet-4-5", "anthropic"));
        assert!(!s.covers(Some("claude-code"), "gpt-5", "anthropic"));
        assert!(!s.covers(Some("claude-code"), "claude-sonnet-4-5", "relay"));
    }

    /// `*` 写在哪儿都行（开头、中间、结尾、好几个），不分大小写；三张单子都一样
    #[test]
    fn a_star_matches_anywhere_in_every_list_regardless_of_case() {
        let cases: &[(&str, &str, bool)] = &[
            ("*", "anything", true),
            ("*", "", true),
            ("claude-*", "claude-sonnet-4-5", true),
            ("*-mini", "gpt-4o-mini", true),
            ("*-mini", "gpt-4o-mini-2024", false),
            ("gpt-*-mini", "gpt-4o-mini", true),
            ("gpt-*-mini", "gpt-4o", false),
            ("*sonnet*", "claude-sonnet-4-5", true),
            ("*sonnet*", "claude-opus-4-5", false),
            ("c*d*e", "claude-code-relay-cde", true),
            ("a*b*c", "acb", false),
            ("**", "x", true),
            ("Claude-*", "CLAUDE-OPUS", true),
            ("*-RELAY", "cn-relay", true),
            ("exact", "exact", true),
            ("exact", "exactly", false),
            ("exact", "EXACT", true),
            (" padded* ", "padded-up", true),
        ];
        for &(pattern, value, want) in cases {
            for (what, s, client, model, upstream) in [
                (
                    "clients",
                    scope(&[pattern], &[], &[]),
                    Some(value),
                    "m",
                    "u",
                ),
                ("models", scope(&[], &[pattern], &[]), Some("c"), value, "u"),
                (
                    "upstreams",
                    scope(&[], &[], &[pattern]),
                    Some("c"),
                    "m",
                    value,
                ),
            ] {
                assert_eq!(
                    s.covers(client, model, upstream),
                    want,
                    "{what}: {pattern:?} on {value:?}"
                );
            }
        }
        // 一张单子里有一个对上就算
        let s = scope(&[], &["gpt-*", "*sonnet*"], &[]);
        assert!(s.covers(None, "claude-sonnet-4-5", "u"));
        assert!(!s.covers(None, "claude-opus-4-5", "u"));
    }

    /// 认不出是哪个应用的请求，挑应用的插件不管它 —— 管了就等于对每个不认识的
    /// 客户端都改请求
    #[test]
    fn an_unknown_client_is_covered_only_by_plugins_that_do_not_pick_clients() {
        assert!(scope(&[], &[], &[]).covers(None, "m", "u"));
        assert!(!scope(&["*"], &[], &[]).covers(None, "m", "u"));
    }

    /// 请求钩子那一段：按配置的顺序；跑不了的也在（调用方照 `on_error` 处置），
    /// 能跑的只要有请求钩子的；停用的、范围外的不在。
    #[test]
    fn the_request_list_keeps_the_order_and_includes_broken_plugins() {
        let mut off = active("off", REQUEST, Scope::default(), None);
        Arc::get_mut(&mut off).unwrap().enabled = false;
        let set = PluginSet::new(vec![
            active("b-first", REQUEST, Scope::default(), None),
            active("reply-only", REPLY, Scope::default(), None),
            active("changed", REPLY, Scope::default(), Some(Broken::Changed)),
            off,
            active("other-model", REQUEST, scope(&[], &["gpt-*"], &[]), None),
            active("a-last", REQUEST, Scope::default(), None),
        ]);
        assert_eq!(
            ids(&set.for_request(
                CONVERSATION,
                Some("claude-code"),
                "claude-opus-4-5",
                "anthropic"
            )),
            ["b-first", "changed", "a-last"]
        );
    }

    /// 发往哪一家定了才挑插件：只管某一家的，发往别家时不跑；**坏了的也一样** —— 它
    /// 只拦发往它那一家的请求，不再因为「还不知道去哪儿」把别家的也拦下
    #[test]
    fn the_request_list_follows_the_upstream_of_the_attempt() {
        let set = PluginSet::new(vec![
            active("only-a", REQUEST, scope(&[], &[], &["relay-a"]), None),
            active(
                "broken-a",
                REQUEST,
                scope(&[], &[], &["relay-a"]),
                Some(Broken::Changed),
            ),
            active("everywhere", REQUEST, Scope::default(), None),
        ]);
        assert_eq!(
            ids(&set.for_request(CONVERSATION, None, "m", "relay-a")),
            ["only-a", "broken-a", "everywhere"]
        );
        assert_eq!(
            ids(&set.for_request(CONVERSATION, None, "m", "relay-b")),
            ["everywhere"]
        );
    }

    /// 模型看的是发出去的那个：规则把 claude 改成 glm 发给中转，管 `glm-*` 的插件管这一次
    #[test]
    fn models_match_the_model_sent_upstream() {
        let set = PluginSet::new(vec![active(
            "glm",
            REQUEST,
            scope(&[], &["glm-*"], &[]),
            None,
        )]);
        assert_eq!(
            ids(&set.for_request(CONVERSATION, None, "glm-4.6", "relay")),
            ["glm"]
        );
        assert!(
            set.for_request(CONVERSATION, None, "claude-sonnet-4-5", "relay")
                .is_empty()
        );
    }

    /// 只列处理这一种请求的插件，**跑不了的也一样**：只处理对话的插件坏了，拦不着嵌入和
    /// 补全；声明了嵌入的坏了，拦的也只是嵌入（和对话，如果也声明了的话）
    #[test]
    fn the_request_list_has_only_plugins_that_handle_the_kind() {
        use tw_api::RequestKind::*;
        let with = |a: Arc<Active>, kinds: &[tw_api::RequestKind]| {
            let mut a = Arc::try_unwrap(a).unwrap();
            a.requests = kinds.to_vec();
            Arc::new(a)
        };
        let set = PluginSet::new(vec![
            active("chat", REQUEST, Scope::default(), None),
            active(
                "chat-broken",
                REQUEST,
                Scope::default(),
                Some(Broken::Changed),
            ),
            with(
                active("embeds", REQUEST, Scope::default(), None),
                &[Conversation, Embeddings],
            ),
            with(
                active(
                    "embeds-broken",
                    REQUEST,
                    Scope::default(),
                    Some(Broken::Changed),
                ),
                &[Embeddings],
            ),
            with(
                active("completes", REQUEST, Scope::default(), None),
                &[Completions],
            ),
        ]);
        assert_eq!(
            ids(&set.for_request(Embeddings, None, "m", "u")),
            ["embeds", "embeds-broken"]
        );
        assert_eq!(
            ids(&set.for_request(Completions, None, "m", "u")),
            ["completes"]
        );
        assert_eq!(
            ids(&set.for_request(Conversation, None, "m", "u")),
            ["chat", "chat-broken", "embeds"]
        );
    }

    #[test]
    fn the_reply_list_has_only_ready_plugins_with_reply_hooks_for_that_upstream() {
        let set = PluginSet::new(vec![
            active("request-only", REQUEST, Scope::default(), None),
            active("text", REPLY, Scope::default(), None),
            active(
                "broken",
                REPLY,
                Scope::default(),
                Some(Broken::Error(tw_types::msg!("t.x" => "x"))),
            ),
            active("elsewhere", REPLY, scope(&[], &[], &["relay-*"]), None),
        ]);
        assert_eq!(ids(&set.for_reply(None, "m", "anthropic")), ["text"]);
        assert_eq!(
            ids(&set.for_reply(None, "m", "relay-cn")),
            ["text", "elsewhere"]
        );
    }

    fn run(outcome: PluginOutcome, cpu_us: u64) -> PluginRun {
        PluginRun {
            plugin_id: "p".into(),
            plugin_name: "p".into(),
            hook: PluginHook::Request,
            outcome,
            error: (outcome == PluginOutcome::Error).then(|| tw_types::msg!("t.boom" => "boom")),
            cpu_us,
            detail: None,
        }
    }

    #[test]
    fn stats_count_runs_and_skips_are_not_calls() {
        let s = Stats::default();
        assert_eq!(s.view(), tw_api::PluginStats::default());
        s.note(&run(PluginOutcome::Unchanged, 100), 1);
        s.note(&run(PluginOutcome::Changed, 200), 2);
        s.note(&run(PluginOutcome::Rejected, 300), 3);
        s.note(&run(PluginOutcome::Error, 400), 4);
        s.note(&run(PluginOutcome::Skipped, 0), 5);
        let v = s.view();
        assert_eq!(
            (v.calls, v.changed, v.rejected, v.errors),
            (4, 1, 1, 1),
            "{v:?}"
        );
        assert_eq!(v.avg_cpu_us, 250);
        let last = v.last_error.expect("the error was not kept");
        assert_eq!((last.at_ms, last.message.code.as_str()), (4, "t.boom"));
    }

    #[test]
    fn the_log_ring_keeps_the_newest_lines() {
        let r = LogRing::default();
        r.extend(1, Some(7), PluginHook::Request, Vec::new());
        assert!(r.lines().is_empty());
        let line = |i: usize| LogLine {
            level: tw_api::PluginLogLevel::Log,
            text: format!("line {i}"),
        };
        r.extend(
            2,
            Some(7),
            PluginHook::Reply,
            (0..LogRing::CAP + 20).map(line).collect(),
        );
        let got = r.lines();
        assert_eq!(got.len(), LogRing::CAP);
        assert_eq!(got[0].text, "line 20", "the oldest lines should go first");
        assert_eq!(
            got.last().unwrap().text,
            format!("line {}", LogRing::CAP + 19)
        );
        assert_eq!(got[0].request_id, Some(7));
        assert_eq!(got[0].hook, PluginHook::Reply);
    }
}
