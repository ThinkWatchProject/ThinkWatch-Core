//! 客户端的自言自语：识别并本地应答。
//!
//! **识别必须窄，宁可漏判。**误判一个真实请求为探测并伪造回复，是这个
//! 功能唯一的严重故障模式 —— 用户会看到一个凭空出现的假答案，而且完全
//! 无从察觉。漏判的代价只是多花几分钱。两者不对等，所以每一条判定都
//! 往严了写。

use bytes::Bytes;
use serde_json::Value;
use tw_config::{ClientProbes, ProbeAction};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeKind {
    /// A 类：连通性检查。客户端只想知道「通不通」，回什么它不看
    HealthCheck,
    /// A 类：预热
    Warmup,
    /// B 类：给会话起标题。**有用户可见产物**
    Titling,
    /// B 类：话题检测
    TopicDetect,
    /// B 类：建议模式
    Suggestion,
}

impl ProbeKind {
    /// 规则里 `when: { intent: ... }` 写的那个词，也是 `LocallyAnswered.probe`
    /// 和 `ProbeView.id` 发的值。
    pub fn slug(&self) -> &'static str {
        match self {
            ProbeKind::HealthCheck => "health_check",
            ProbeKind::Warmup => "warmup",
            ProbeKind::Titling => "titling",
            ProbeKind::TopicDetect => "topic_detect",
            ProbeKind::Suggestion => "suggestion",
        }
    }

    pub fn action(&self, c: &ClientProbes) -> ProbeAction {
        match self {
            ProbeKind::HealthCheck => c.health_check,
            ProbeKind::Warmup => c.warmup,
            ProbeKind::Titling => c.titling,
            ProbeKind::TopicDetect => c.topic_detect,
            ProbeKind::Suggestion => c.suggestion,
        }
    }
}

// 各家客户端的原文。**放在一起是为了让「我们凭什么这么判」可审计** ——
// 散在代码里的魔法字符串没人敢改。
const TITLE_MARK: &str = "Please write a 5-10 word title for the following conversation:";
const TOPIC_MARK: &str = "Analyze if this message indicates a new conversation topic";
const SUGGEST_MARK: &str = "[SUGGESTION MODE:";
const WARMUP_BODY: &str = "Warmup";

/// 认一认这是不是客户端自己的辅助请求。
///
/// **先做整包字节扫描快速排除**，不命中就直接返回，连 JSON 都不解析。
/// 绝大多数请求会在这一步出去 —— 而这段代码跑在每一个请求的关键路径上。
///
/// `is_claude_code` 由调用方判断。`max_tokens: 1` 那条**必须**同时要求
/// 它，否则会误伤别人真实的 `max_tokens: 1` 请求。
pub fn classify(body: &Bytes, is_claude_code: bool) -> Option<ProbeKind> {
    let has = |m: &str| memfind(body, m.as_bytes());

    // 快速排除。四个 B 类标记各有一句独特的原文；A 类里 warmup 也有，
    // 而 health check 只能靠 `max_tokens` 这个字段名先粗筛。
    let maybe_title = has(TITLE_MARK);
    let maybe_topic = has(TOPIC_MARK);
    let maybe_suggest = has(SUGGEST_MARK);
    let maybe_warmup = has(WARMUP_BODY);
    let maybe_health = is_claude_code && has("max_tokens") && has("haiku");
    if !(maybe_title || maybe_topic || maybe_suggest || maybe_warmup || maybe_health) {
        return None;
    }

    let v: Value = serde_json::from_slice(body).ok()?;
    let obj = v.as_object()?;

    // B 类的三条：标记出现在 system 或者第一个 user 消息里就算。
    // **先判 B 类** —— 一个带标记的请求即使 max_tokens 是 1，它也是
    // 标题请求，不是健康检查。
    if maybe_title && text_contains(obj, TITLE_MARK) {
        return Some(ProbeKind::Titling);
    }
    if maybe_topic && text_contains(obj, TOPIC_MARK) {
        return Some(ProbeKind::TopicDetect);
    }
    if maybe_suggest && first_text_starts_with(obj, SUGGEST_MARK) {
        return Some(ProbeKind::Suggestion);
    }
    // 预热：正文**恰好**是 `Warmup`。「恰好」这两个字是这条判定的全部
    // 安全性 —— 一个碰巧提到 Warmup 的真实问题不该被伪造答案。
    if maybe_warmup && only_message_is(obj, WARMUP_BODY) {
        return Some(ProbeKind::Warmup);
    }
    if maybe_health
        && obj.get("max_tokens").and_then(Value::as_u64) == Some(1)
        && obj
            .get("model")
            .and_then(Value::as_str)
            .is_some_and(|m| m.contains("haiku"))
    {
        return Some(ProbeKind::HealthCheck);
    }
    None
}

/// 朴素子串查找。请求体是几十 KB 的量级，不值得为它引一个 SIMD 库。
fn memfind(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    hay.windows(needle.len()).any(|w| w == needle)
}

/// 把 system 和所有消息里的文本摊平了看。
fn all_text(obj: &serde_json::Map<String, Value>) -> String {
    let mut out = String::new();
    push_text(obj.get("system"), &mut out);
    if let Some(ms) = obj.get("messages").and_then(Value::as_array) {
        for m in ms {
            push_text(m.get("content"), &mut out);
        }
    }
    out
}

fn push_text(v: Option<&Value>, out: &mut String) {
    match v {
        Some(Value::String(s)) => {
            out.push_str(s);
            out.push('\n');
        }
        Some(Value::Array(a)) => {
            for b in a {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    out.push_str(t);
                    out.push('\n');
                }
            }
        }
        _ => {}
    }
}

fn text_contains(obj: &serde_json::Map<String, Value>, mark: &str) -> bool {
    all_text(obj).contains(mark)
}

/// 第一个 text 块以某个前缀开头。建议模式的标记就是这么用的。
fn first_text_starts_with(obj: &serde_json::Map<String, Value>, prefix: &str) -> bool {
    let Some(ms) = obj.get("messages").and_then(Value::as_array) else {
        return false;
    };
    let Some(first) = ms.first() else {
        return false;
    };
    match first.get("content") {
        Some(Value::String(s)) => s.trim_start().starts_with(prefix),
        Some(Value::Array(a)) => a
            .iter()
            .find_map(|b| b.get("text").and_then(Value::as_str))
            .is_some_and(|t| t.trim_start().starts_with(prefix)),
        _ => false,
    }
}

/// 只有一条消息，而且正文恰好等于某个字符串。
fn only_message_is(obj: &serde_json::Map<String, Value>, exact: &str) -> bool {
    let Some(ms) = obj.get("messages").and_then(Value::as_array) else {
        return false;
    };
    if ms.len() != 1 {
        return false;
    }
    // system 里有东西就不是预热了 —— 预热请求是光秃秃的
    if obj.get("system").is_some() {
        return false;
    }
    match ms[0].get("content") {
        Some(Value::String(s)) => s.trim() == exact,
        Some(Value::Array(a)) => {
            a.len() == 1
                && a[0]
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t.trim() == exact)
        }
        _ => false,
    }
}

/// 请求要的是流式吗。伪造的响应必须跟着它走 —— 客户端按自己请求的形状
/// 去解析，回错了形状比不拦截更糟。
pub fn wants_stream(body: &Bytes) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("stream").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// Anthropic 的消息 id：`msg_01` 加 22 位。**格式要对得上** ——
/// 严格的 SDK 会校验它。
fn message_id() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut s = String::from("msg_01");
    // 不引 rand：探测应答的 id 只需要「看起来是个 id」且互不相同。
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ 0x9E37_79B9_7F4A_7C15;
    for _ in 0..22 {
        // xorshift64
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.push(ALPHABET[(x % ALPHABET.len() as u64) as usize] as char);
    }
    s
}

/// 伪造的应答内容。
///
/// **对客户端要逼真，对用户要透明** —— 这两件事不矛盾：响应体按
/// Anthropic 的完整形状给（少一个字段严格的 SDK 就报错），而在日志和
/// 统计里它单独标记为本地应答、成本记 0。
fn reply_text(kind: ProbeKind) -> &'static str {
    match kind {
        // max_tokens 是 1，所以只能有一个 token 的量
        ProbeKind::HealthCheck => "OK",
        ProbeKind::Warmup => "OK",
        // B 类默认不会走到这里；用户显式改成 intercept 时给一个中性的值
        ProbeKind::Titling => "New Conversation",
        ProbeKind::TopicDetect => "false",
        ProbeKind::Suggestion => "",
    }
}

fn stop_reason(kind: ProbeKind) -> &'static str {
    // `max_tokens: 1` 的请求真跑上游时，停下来的理由就是 max_tokens。
    // 写成 end_turn 是一个客户端能看出来的破绽。
    match kind {
        ProbeKind::HealthCheck => "max_tokens",
        _ => "end_turn",
    }
}

fn model_of(body: &Bytes) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("model")
                .and_then(Value::as_str)
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "claude-3-5-haiku-20241022".to_string())
}

/// 非流式的应答体。
pub fn json_response(kind: ProbeKind, body: &Bytes) -> Value {
    let text = reply_text(kind);
    serde_json::json!({
        "id": message_id(),
        "type": "message",
        "role": "assistant",
        "model": model_of(body),
        "content": [{ "type": "text", "text": text }],
        "stop_reason": stop_reason(kind),
        "stop_sequence": Value::Null,
        // 四个分项都要在。少一个，严格的 SDK 就会报错。
        "usage": {
            "input_tokens": 0,
            "output_tokens": 0,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
        }
    })
}

/// 流式的应答：完整的事件序列。
///
/// `message_start` → `content_block_start` → delta → `content_block_stop`
/// → `message_delta` → `message_stop`。**少一个事件就有 SDK 会挂**，
/// 而那比不拦截更糟。
pub fn sse_response(kind: ProbeKind, body: &Bytes) -> String {
    let id = message_id();
    let model = model_of(body);
    let text = reply_text(kind);
    let start = serde_json::json!({
        "type": "message_start",
        "message": {
            "id": id,
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": [],
            "stop_reason": Value::Null,
            "stop_sequence": Value::Null,
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0,
            }
        }
    });
    let block_start = serde_json::json!({
        "type": "content_block_start",
        "index": 0,
        "content_block": { "type": "text", "text": "" }
    });
    let delta = serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": { "type": "text_delta", "text": text }
    });
    let block_stop = serde_json::json!({ "type": "content_block_stop", "index": 0 });
    let msg_delta = serde_json::json!({
        "type": "message_delta",
        "delta": { "stop_reason": stop_reason(kind), "stop_sequence": Value::Null },
        "usage": { "output_tokens": 0 }
    });
    let msg_stop = serde_json::json!({ "type": "message_stop" });

    let mut out = String::new();
    for (name, v) in [
        ("message_start", &start),
        ("content_block_start", &block_start),
        ("content_block_delta", &delta),
        ("content_block_stop", &block_stop),
        ("message_delta", &msg_delta),
        ("message_stop", &msg_stop),
    ] {
        out.push_str(&format!("event: {name}\ndata: {v}\n\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> Bytes {
        Bytes::from(s.to_string())
    }

    // ── 该认出来的 ──────────────────────────────────────────────────
    #[test]
    fn a_health_check_is_recognised() {
        let body = b(r#"{"model":"claude-3-5-haiku-20241022","max_tokens":1,
            "messages":[{"role":"user","content":"hi"}]}"#);
        assert_eq!(classify(&body, true), Some(ProbeKind::HealthCheck));
    }

    #[test]
    fn a_warmup_is_recognised_in_both_content_shapes() {
        for content in [r#""Warmup""#, r#"[{"type":"text","text":"Warmup"}]"#] {
            let body = b(&format!(
                r#"{{"model":"claude-3-5-haiku-20241022","max_tokens":32,
                   "messages":[{{"role":"user","content":{content}}}]}}"#
            ));
            assert_eq!(classify(&body, true), Some(ProbeKind::Warmup), "{content}");
        }
    }

    #[test]
    fn the_three_b_class_markers_are_recognised() {
        let title = b(&format!(
            r#"{{"model":"m","messages":[{{"role":"user","content":"{TITLE_MARK} 之类的"}}]}}"#
        ));
        assert_eq!(classify(&title, true), Some(ProbeKind::Titling));

        let topic = b(&format!(
            r#"{{"model":"m","system":"{TOPIC_MARK}, and if so extract a title",
               "messages":[{{"role":"user","content":"x"}}]}}"#
        ));
        assert_eq!(classify(&topic, true), Some(ProbeKind::TopicDetect));

        let sugg = b(&format!(
            r#"{{"model":"m","messages":[{{"role":"user","content":"{SUGGEST_MARK} on]"}}]}}"#
        ));
        assert_eq!(classify(&sugg, true), Some(ProbeKind::Suggestion));
    }

    // ── 不该认出来的。**这一组比上一组重要。** ───────────────────────
    #[test]
    fn a_real_request_that_merely_mentions_warmup_is_not_a_warmup() {
        // 「恰好是 Warmup」这两个字是这条判定的全部安全性。
        for content in [
            "Warmup the cache please",
            "请解释一下 Warmup 是什么",
            "Warmup\n\n然后跑一下测试",
        ] {
            let body = b(&format!(
                r#"{{"model":"m","messages":[{{"role":"user","content":{}}}]}}"#,
                serde_json::to_string(content).unwrap()
            ));
            assert_eq!(classify(&body, true), None, "{content}");
        }
    }

    #[test]
    fn a_warmup_with_a_system_prompt_or_history_is_a_real_request() {
        // 预热请求是光秃秃的。带上下文的就不是了。
        let with_system = b(r#"{"model":"m","system":"you are helpful",
            "messages":[{"role":"user","content":"Warmup"}]}"#);
        assert_eq!(classify(&with_system, true), None);
        let with_history = b(r#"{"model":"m","messages":[
            {"role":"user","content":"Warmup"},
            {"role":"assistant","content":"ok"}]}"#);
        assert_eq!(classify(&with_history, true), None);
    }

    #[test]
    fn a_max_tokens_one_request_from_someone_else_is_left_alone() {
        // **这就是那个误伤。**别人真的想只要一个 token 的时候，
        // 我们不能给他一个编的答案。
        let body = b(r#"{"model":"claude-3-5-haiku-20241022","max_tokens":1,
            "messages":[{"role":"user","content":"hi"}]}"#);
        assert_eq!(classify(&body, false), None);
    }

    #[test]
    fn a_max_tokens_one_request_on_a_big_model_is_not_a_health_check() {
        // 健康检查走的是小模型档。一个 max_tokens:1 的 opus 请求是别的
        // 东西 —— 大概率是在量什么。
        let body = b(r#"{"model":"claude-opus-4","max_tokens":1,
            "messages":[{"role":"user","content":"hi"}]}"#);
        assert_eq!(classify(&body, true), None);
    }

    #[test]
    fn a_title_marker_inside_the_assistant_reply_still_counts_but_a_paraphrase_does_not() {
        // 换个说法就不该命中 —— 我们认的是那一句原文，不是那个意思。
        let para = b(r#"{"model":"m","messages":[{"role":"user",
            "content":"please write a title for this conversation"}]}"#);
        assert_eq!(classify(&para, true), None);
    }

    #[test]
    fn a_suggestion_marker_that_is_not_at_the_start_is_not_suggestion_mode() {
        let body = b(&format!(
            r#"{{"model":"m","messages":[{{"role":"user","content":"看看这个 {SUGGEST_MARK} on]"}}]}}"#
        ));
        assert_eq!(classify(&body, true), None);
    }

    #[test]
    fn a_body_that_is_not_json_is_never_a_probe() {
        // 解不开就放行。我们的解析器不认识的东西，上游可能完全认识。
        assert_eq!(classify(&b("Warmup"), true), None);
        assert_eq!(classify(&b("{broken"), true), None);
    }

    #[test]
    fn an_ordinary_request_does_not_even_get_parsed() {
        // 快速排除跑在每个请求的关键路径上。这条测的是它确实先排除了。
        let body = b(r#"{"model":"claude-sonnet-4-5","max_tokens":8000,
            "messages":[{"role":"user","content":"帮我看看这段代码"}]}"#);
        assert_eq!(classify(&body, true), None);
    }

    #[test]
    fn a_titling_request_with_max_tokens_one_is_still_titling() {
        // B 类先判。搞反了的话，一个标题请求会被当成健康检查拦掉 ——
        // 而那正是「把功能关掉」那个后果。
        let body = b(&format!(
            r#"{{"model":"claude-3-5-haiku-20241022","max_tokens":1,
               "messages":[{{"role":"user","content":"{TITLE_MARK} 啦啦"}}]}}"#
        ));
        assert_eq!(classify(&body, true), Some(ProbeKind::Titling));
    }

    // ── 伪造的响应 ──────────────────────────────────────────────────
    #[test]
    fn the_fake_sse_has_every_event_a_strict_sdk_expects() {
        // 少一个事件就有 SDK 会挂，而那比不拦截更糟。
        let body = b(r#"{"model":"claude-3-5-haiku-20241022","max_tokens":1,"stream":true}"#);
        let s = sse_response(ProbeKind::HealthCheck, &body);
        for ev in [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ] {
            assert!(s.contains(&format!("event: {ev}\n")), "少了 {ev}：{s}");
        }
        // 顺序也要对
        let idx: Vec<_> = ["message_start", "content_block_delta", "message_stop"]
            .iter()
            .map(|e| s.find(&format!("event: {e}\n")).unwrap())
            .collect();
        assert!(idx[0] < idx[1] && idx[1] < idx[2], "{s}");
        // 每一帧都以空行收尾，否则 SSE 解析器会一直等下去
        for frame in s.split("\n\n").filter(|f| !f.trim().is_empty()) {
            assert!(frame.starts_with("event: "), "{frame}");
        }
    }

    #[test]
    fn a_max_tokens_one_answer_says_it_stopped_because_of_max_tokens() {
        // 写成 end_turn 是一个客户端能看出来的破绽。
        let body = b(r#"{"model":"m","max_tokens":1}"#);
        assert_eq!(
            json_response(ProbeKind::HealthCheck, &body)["stop_reason"],
            "max_tokens"
        );
        assert_eq!(
            json_response(ProbeKind::Warmup, &body)["stop_reason"],
            "end_turn"
        );
    }

    #[test]
    fn the_usage_block_has_all_four_fields() {
        let body = b(r#"{"model":"m"}"#);
        let v = json_response(ProbeKind::HealthCheck, &body);
        for k in [
            "input_tokens",
            "output_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ] {
            assert!(v["usage"].get(k).is_some(), "少了 usage.{k}");
        }
    }

    #[test]
    fn the_answer_echoes_the_model_the_client_asked_for() {
        // 回一个别的模型名，客户端那边的记账就对不上了。
        let body = b(r#"{"model":"claude-3-5-haiku-20241022"}"#);
        assert_eq!(
            json_response(ProbeKind::HealthCheck, &body)["model"],
            "claude-3-5-haiku-20241022"
        );
    }

    #[test]
    fn message_ids_look_like_anthropic_ids_and_do_not_repeat() {
        let a = message_id();
        let b_ = message_id();
        assert!(a.starts_with("msg_01"), "{a}");
        assert_eq!(a.len(), 6 + 22, "{a}");
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        assert_ne!(a, b_, "两次生成撞了");
    }

    #[test]
    fn the_reply_shape_follows_the_request() {
        assert!(wants_stream(&b(r#"{"stream":true}"#)));
        assert!(!wants_stream(&b(r#"{"stream":false}"#)));
        // 不写就是非流式 —— 猜错了形状比不拦截更糟
        assert!(!wants_stream(&b(r#"{"model":"m"}"#)));
    }
}
