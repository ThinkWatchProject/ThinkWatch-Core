//! 认出「这几十个请求是同一次任务」。
//!
//! Claude Code 的一次任务是几十到上百个请求，携带不断增长的上下文。
//! **孤立地看单个请求，看不出任何有用的东西** —— 「我一次重构花了 $4.7，
//! 其中 $3.1 是反复读同一个大文件」这种洞察只有会话视图能给。
//!
//! 判据是：**system prompt 指纹 + 首条 user message 指纹**，
//! 再加时间窗聚类（[`Sessions`]：同一个指纹隔太久再出现，算新的一次）。
//! 四种格式都认；请求自己带着会话标识（`prompt_cache_key`）时直接用它。
//!
//! **会话在请求开始的那一刻就定下来，只定一次**：开始事件带着它，落库的那一行
//! 记的也是它。以前事件里只有指纹，归到哪一次要等落库时再算 —— 一个还在跑的
//! 请求就说不出自己属于哪次会话，界面上只能是散着的一行。
//!
//! 为什么这两个指纹够用：一次对话里，客户端每一轮都把完整历史发上来，
//! 所以**开头那两段在整个会话里逐字不变**，而不同任务的开头几乎不会
//! 撞上。不完美，但对单机单用户场景足够 —— 而「足够」是这里的正确目标：
//! 为了那点边角情形去做精确的会话跟踪，要么得改协议，要么得让客户端配合。

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

/// 隔多久算另一次任务。
///
/// **半小时是按「人」定的，不是按机器**：中间去开了个会再回来接着改，
/// 那多半还是同一件事；隔了一夜再打开同一个仓库，那通常不是。切错的
/// 代价是对称的（并多了或分多了），所以取一个人能理解的整数。
pub const SESSION_GAP_MS: u64 = 30 * 60 * 1000;

/// 记着几段对话就清一次早就断了的。**清掉的不会改变任何结果**：隔了这么久再
/// 出现，本来就要算新的一次
const PRUNE_AT: usize = 4096;

/// 每个对话指纹当前归到哪一次会话，以及它最后一次出现是什么时候。
///
/// **只在内存里。**core 重启之后，同一段对话会被算成新的一次任务 ——
/// 那不理想，但比把它持久化成又一份状态好：会话是个观测概念，不是
/// 事实来源。**跨配置重载存活**：改一条规则不该把正在进行的任务切成两段。
#[derive(Default)]
pub struct Sessions {
    open: Mutex<HashMap<String, (String, u64)>>,
}

impl Sessions {
    /// 这个指纹、在这一刻开始的请求，归到哪一次会话。
    ///
    /// **会话 id 里带着起始时刻**（`{指纹}-{毫秒}`），所以同一段对话隔天再聊会
    /// 得到两条记录 —— 而那正是我们想要的：它们是两次任务。
    pub fn assign(&self, fp: &str, at_ms: u64) -> String {
        // 锁中毒了照样拿里面的表：少并一段会话，好过一个不转发的网关
        let mut open = self.open.lock().unwrap_or_else(|p| p.into_inner());
        if open.len() >= PRUNE_AT {
            open.retain(|_, (_, last)| at_ms.saturating_sub(*last) <= SESSION_GAP_MS);
        }
        let fresh = format!("{fp}-{at_ms}");
        let e = open
            .entry(fp.to_string())
            .or_insert_with(|| (fresh.clone(), at_ms));
        if at_ms.saturating_sub(e.1) > SESSION_GAP_MS {
            *e = (fresh, at_ms);
        } else {
            e.1 = e.1.max(at_ms);
        }
        e.0.clone()
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.open.lock().unwrap().len()
    }
}

/// 一段文本的短指纹。
fn fp(s: &str) -> String {
    blake3::hash(s.as_bytes()).to_hex()[..12].to_string()
}

/// 内容块数组里的文本，一块一行。
fn text_of(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 系统提示摊平成文本：Anthropic 的 `system`、Responses 的 `instructions`（字符串
/// 或内容块数组），Gemini 的 `systemInstruction`（REST 也收下划线写法，正文在
/// `parts` 里）。Chat 的系统提示写在 messages 里，只看首条 user 就够了。
fn system_text(v: &Value) -> String {
    match v.get("system").or_else(|| v.get("instructions")) {
        Some(Value::String(s)) => return s.clone(),
        Some(Value::Array(a)) => return text_of(a),
        _ => {}
    }
    v.get("systemInstruction")
        .or_else(|| v.get("system_instruction"))
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
        .map(|a| text_of(a))
        .unwrap_or_default()
}

/// 首条 user 消息的文本。对话在 Anthropic 和 Chat 里是 `messages`，Responses 里是
/// `input`（也可以直接是一句话），Gemini 里是 `contents`，正文在 `parts` 里。
fn first_user_text(v: &Value) -> String {
    if let Some(Value::String(s)) = v.get("input") {
        return s.clone();
    }
    let Some(turns) = ["messages", "input", "contents"]
        .iter()
        .find_map(|k| v.get(*k).and_then(Value::as_array))
    else {
        return String::new();
    };
    let Some(first) = turns
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
    else {
        return String::new();
    };
    match first.get("content").or_else(|| first.get("parts")) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => text_of(a),
        _ => String::new(),
    }
}

/// 这次请求属于哪一段对话。
///
/// **请求自己带着会话标识时用它**：`prompt_cache_key`，Codex 每段对话一个（就是
/// 对话的 id）。同一个仓库里开的几段 Codex 对话，开头的指令和环境说明一字不差，
/// 只看开头会把它们并成一段。没带的按系统提示和首条 user 消息认。
///
/// `None` = 认不出来。**认不出来就说认不出来** —— 硬凑一个指纹会把一堆
/// 互不相干的请求并成一个「会话」，那比没有会话视图更糟。
pub fn fingerprint(v: &Value) -> Option<String> {
    if let Some(key) = v
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty())
    {
        // 和按文本认的一个长相：两段 12 位
        let h = blake3::hash(key.as_bytes()).to_hex();
        return Some(format!("{}-{}", &h[..12], &h[12..24]));
    }
    let sys = system_text(v);
    let user = first_user_text(v);
    // 两段都空 = 这个请求里没有任何能认人的东西
    if sys.is_empty() && user.is_empty() {
        return None;
    }
    Some(format!("{}-{}", fp(&sys), fp(&user)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(sys: &str, first: &str, extra_turns: usize) -> Value {
        let mut msgs = vec![json!({"role": "user", "content": first})];
        for i in 0..extra_turns {
            msgs.push(json!({"role": "assistant", "content": format!("回复 {i}")}));
            msgs.push(json!({"role": "user", "content": format!("再来 {i}")}));
        }
        json!({"model": "claude-sonnet-4-5", "system": sys, "messages": msgs})
    }

    #[test]
    fn the_same_conversation_keeps_its_fingerprint_as_it_grows() {
        // 这是整个机制成立的前提：客户端每轮都把完整历史发上来，
        // 所以开头那两段逐字不变。
        let a = fingerprint(&body("你是一个助手", "帮我重构这个文件", 0)).unwrap();
        for turns in [1, 5, 40] {
            let b = fingerprint(&body("你是一个助手", "帮我重构这个文件", turns)).unwrap();
            assert_eq!(a, b, "第 {turns} 轮的指纹变了");
        }
    }

    #[test]
    fn different_tasks_get_different_fingerprints() {
        let a = fingerprint(&body("你是一个助手", "帮我重构这个文件", 3)).unwrap();
        let b = fingerprint(&body("你是一个助手", "帮我写个测试", 3)).unwrap();
        assert_ne!(a, b);
        // system prompt 变了也算另一段 —— 换了个客户端或者换了个模式
        let c = fingerprint(&body("你是一个代码审查员", "帮我重构这个文件", 3)).unwrap();
        assert_ne!(a, c);
    }

    #[test]
    fn a_system_prompt_written_as_content_blocks_works_the_same() {
        // Claude Code 发的是数组形式，而且带 cache_control。
        let blocks = json!({
            "system": [
                {"type": "text", "text": "你是一个助手", "cache_control": {"type": "ephemeral"}}
            ],
            "messages": [{"role": "user", "content": "帮我重构这个文件"}]
        });
        let plain = body("你是一个助手", "帮我重构这个文件", 0);
        assert_eq!(
            fingerprint(&blocks),
            fingerprint(&plain),
            "两种写法该是同一段对话"
        );
    }

    #[test]
    fn a_request_with_nothing_to_go_on_says_so_rather_than_inventing_one() {
        // 硬凑一个指纹会把一堆互不相干的请求并成一个「会话」，
        // 那比没有会话视图更糟。
        assert_eq!(fingerprint(&json!({"model": "x"})), None);
        assert_eq!(fingerprint(&json!({"model": "x", "messages": []})), None);
        assert_eq!(fingerprint(&json!({"system": "", "messages": []})), None);
    }

    #[test]
    fn an_assistant_only_history_still_finds_the_first_user_turn() {
        let v = json!({
            "messages": [
                {"role": "assistant", "content": "我先说话"},
                {"role": "user", "content": "真正的第一句"}
            ]
        });
        let w = json!({"messages": [{"role": "user", "content": "真正的第一句"}]});
        assert_eq!(fingerprint(&v), fingerprint(&w));
    }

    /// 同一个指纹、没隔太久：同一次会话，id 是头一个请求开始的时刻；**隔的是离上一个
    /// 请求多久**，不是离会话开始多久 —— 一次连着改了两个小时的任务还是一次
    #[test]
    fn a_conversation_stays_one_session_until_it_goes_quiet_for_the_gap() {
        let s = Sessions::default();
        let min = 60_000;
        let first = s.assign("fp", 1_000);
        assert_eq!(first, "fp-1000");
        for i in 1..=8 {
            assert_eq!(s.assign("fp", 1_000 + i * 20 * min), first, "第 {i} 个请求");
        }
        let last = 1_000 + 8 * 20 * min;
        // 隔半小时整还算同一次；从那个请求起再隔半小时零一毫秒，就是新的一次
        assert_eq!(s.assign("fp", last + SESSION_GAP_MS), first);
        let later = last + 2 * SESSION_GAP_MS + 1;
        assert_eq!(s.assign("fp", later), format!("fp-{later}"));
        // 另一段对话各算各的
        assert_eq!(s.assign("other", later), format!("other-{later}"));
    }

    /// 并发的请求不一定按开始的先后拿到号：**晚到的一个早一点的时刻不把「最后一次」
    /// 往回拨**
    #[test]
    fn an_out_of_order_request_does_not_rewind_the_last_seen_time() {
        let s = Sessions::default();
        let first = s.assign("fp", 10 * SESSION_GAP_MS);
        assert_eq!(s.assign("fp", 10 * SESSION_GAP_MS + 1_000), first);
        assert_eq!(s.assign("fp", 10 * SESSION_GAP_MS + 500), first);
        assert_eq!(
            s.assign("fp", 10 * SESSION_GAP_MS + 1_000 + SESSION_GAP_MS),
            first,
            "最后一次是 +1000 那一个，不是 +500"
        );
    }

    /// 表不会一直长：早就断了的对话被清掉，而清掉它们不改变任何结果
    #[test]
    fn long_quiet_conversations_are_forgotten_without_changing_any_answer() {
        let s = Sessions::default();
        for i in 0..PRUNE_AT as u64 {
            s.assign(&format!("old-{i}"), 0);
        }
        assert_eq!(s.tracked(), PRUNE_AT);
        let now = 3 * SESSION_GAP_MS;
        assert_eq!(s.assign("new", now), format!("new-{now}"));
        assert_eq!(s.tracked(), 1, "早就断了的对话还留着");
        assert_eq!(s.assign("old-1", now), format!("old-1-{now}"));
    }

    /// Codex 发的 Responses 格式：系统提示在 `instructions`，对话在 `input`
    fn responses(instructions: &str, first: &str, extra_turns: usize) -> Value {
        let mut input = vec![json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": first}]
        })];
        for i in 0..extra_turns {
            input.push(json!({"type": "function_call", "name": "shell", "arguments": "{}", "call_id": format!("c{i}")}));
            input.push(
                json!({"type": "function_call_output", "call_id": format!("c{i}"), "output": "ok"}),
            );
        }
        json!({"model": "gpt-5.5", "instructions": instructions, "input": input})
    }

    #[test]
    fn a_responses_conversation_keeps_its_fingerprint_as_it_grows() {
        let a = fingerprint(&responses("You are Codex", "fix the build", 0)).unwrap();
        for turns in [1, 5, 40] {
            let b = fingerprint(&responses("You are Codex", "fix the build", turns)).unwrap();
            assert_eq!(a, b, "第 {turns} 轮的指纹变了");
        }
        let c = fingerprint(&responses("You are Codex", "write a test", 3)).unwrap();
        assert_ne!(a, c);
        // `input` 直接是一句话，和写成一条消息是同一段
        let plain = json!({"instructions": "You are Codex", "input": "fix the build"});
        assert_eq!(fingerprint(&plain), Some(a));
    }

    #[test]
    fn a_request_that_names_its_conversation_is_taken_at_its_word() {
        // 同一个仓库里的两段 Codex 对话：开头一字不差，靠 `prompt_cache_key` 分开
        let mut one = responses("You are Codex", "<environment_context>", 2);
        one["prompt_cache_key"] = json!("019a-conversation-one");
        let mut two = responses("You are Codex", "<environment_context>", 2);
        two["prompt_cache_key"] = json!("019a-conversation-two");
        assert_ne!(fingerprint(&one), fingerprint(&two));
        // 同一段对话越聊越长，还是它
        let mut later = responses("You are Codex", "<environment_context>", 30);
        later["prompt_cache_key"] = json!("019a-conversation-one");
        assert_eq!(fingerprint(&one), fingerprint(&later));
        // 空的不算数，照样按文本认
        let mut empty = responses("You are Codex", "fix the build", 0);
        empty["prompt_cache_key"] = json!("");
        assert_eq!(
            fingerprint(&empty),
            fingerprint(&responses("You are Codex", "fix the build", 0))
        );
        // 标识本身也不进指纹原文
        let f = fingerprint(&one).unwrap();
        assert!(!f.contains("conversation"), "{f}");
        assert_eq!(f.len(), 25, "和按文本认的一个长相：{f}");
    }

    #[test]
    fn a_gemini_conversation_keeps_its_fingerprint_as_it_grows() {
        let body = |turns: usize| {
            let mut contents = vec![json!({"role": "user", "parts": [{"text": "重构这个模块"}]})];
            for i in 0..turns {
                contents.push(json!({"role": "model", "parts": [{"text": format!("回复 {i}")}]}));
                contents.push(json!({"role": "user", "parts": [{"text": format!("再来 {i}")}]}));
            }
            json!({
                "systemInstruction": {"parts": [{"text": "You are a coding agent"}]},
                "contents": contents
            })
        };
        let a = fingerprint(&body(0)).unwrap();
        assert_eq!(fingerprint(&body(12)), Some(a.clone()));
        // REST 的下划线写法是同一个东西
        let snake = json!({
            "system_instruction": {"parts": [{"text": "You are a coding agent"}]},
            "contents": [{"role": "user", "parts": [{"text": "重构这个模块"}]}]
        });
        assert_eq!(fingerprint(&snake), Some(a));
    }

    #[test]
    fn the_fingerprint_carries_no_readable_content() {
        // 指纹会进数据库，而 system prompt 里常常有用户的私有上下文。
        let v = body("公司内部的机密提示词", "我的私密问题", 2);
        let f = fingerprint(&v).unwrap();
        assert!(!f.contains("机密"), "{f}");
        assert!(!f.contains("私密"), "{f}");
        assert_eq!(f.len(), 25, "两段 12 位加一个连字符：{f}");
    }
}
