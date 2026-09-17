//! 回放测试集。
//!
//! 风险表里写着「上游方言的持续漂移」，而这一路积累的证据全都指向同一个
//! 方向：Codex 在一个 patch 版本里改了 `auth.json` 的语义；
//! `x-stainless-timeout` 透传给上游会提前断流；同一个字段在不同上游叫
//! `reasoning_content` / `reasoning` / `reasoning_details`。
//!
//! **这类故障有一个共同点：我们的单元测试全绿，用户照样挂。**因为漂移
//! 发生在上游，不在我们的代码里。普通测试防不住它 —— 只有拿真实流量
//! 反复回放才行。
//!
//! # 我们有一个别人没有的优势
//!
//! cc-switch 和 sub2api 要专门构造 fixture。**我们是网关，每一个请求和
//! 响应本来就在存储里。**所以「录制」不是一个新功能，是把已有的观测数据
//! 变成测试夹具。
//!
//! # 判据不是字节相等
//!
//! 上游响应里有时间戳、request id、随机的 message id。**逐字节比对会每次
//! 都失败，然后所有人开始忽略它** —— 这是测试集死掉最常见的方式。
//!
//! 比对的是**提取出来的语义**：四个 token 维度、`stop_reason`、工具调用
//! 的名字、错误落到哪个分类、以及从请求里读出来的那些路由事实。这些才是
//! 我们真正依赖的东西。
//!
//! # 脱敏是录制的一部分，不是事后补的
//!
//! 录下来的是真实请求，里面有 API key、有用户的代码、有 system prompt。
//! **落盘之前就走一遍脱敏管线**，而不是先存原始再想办法清洗。
//! 理由比那条日志脱敏更硬：**测试夹具会进 git**，一个装满真实密钥
//! 的 fixture 目录被 push 上去，就再也收不回来了。

use serde::{Deserialize, Serialize};

/// 一个回放用例。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fixture {
    pub version: u32,
    /// 人给的名字，也是文件名
    pub name: String,
    /// 录的时候是什么情况。**只是描述，不参与比对**
    pub note: String,
    pub recorded_at_ms: u64,
    pub request: Recorded,
    pub response: Recorded,
    /// 录制那一刻我们提取出来的语义。**回放时比的就是它**
    pub expect: Extracted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recorded {
    pub path: String,
    /// `application/json` 或 `text/event-stream`
    pub content_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// **已脱敏**的原文
    pub body: String,
}

/// 从一对请求/响应里提取出来的语义。
///
/// 每一项都是我们**真正依赖**的东西 —— 加一项之前先问：它变了会不会
/// 让用户看到错的数字或者走错的路。不会的话就别加，比对项越多越脆。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Extracted {
    // ---- 从请求里（客户端回放：防我们自己改坏了）
    pub model: String,
    pub input_tokens_estimate: u64,
    pub cache: bool,
    pub tools: bool,
    pub tool_count: usize,
    pub image: bool,
    pub thinking: bool,
    pub stream: bool,
    // ---- 从响应里（上游回放：防上游变了）
    /// 四个 token 维度。`None` = 这条响应里没有 usage
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<[u64; 4]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// 响应里出现的工具调用名字，按出现顺序
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<String>,
    /// 上游报的错落到哪个分类。**错误映射变了是真事故** —— 一个 429
    /// 被塌成 502，客户端就不会退避了
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
}

/// 从一对请求/响应里提取语义。**这个函数就是判据本身。**
pub fn extract(request: &Recorded, response: &Recorded) -> Extracted {
    let mut out = Extracted::default();

    // ---- 请求侧：直接复用路由用的那个解析器。**必须是同一个** ——
    // 另写一份提取逻辑的话，回放验的是那一份，而线上跑的是这一份
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&request.body) {
        let f = tw_engine::RequestFacts::from_anthropic_body(&v);
        out.model = f.model;
        out.input_tokens_estimate = f.input_tokens;
        out.cache = f.cache;
        out.tools = f.tools;
        out.tool_count = f.tool_count;
        out.image = f.image;
        out.thinking = f.thinking;
        out.stream = f.stream;
    }

    // ---- 响应侧：同样复用线上那个嗅探器
    let mut sniffer = crate::usage::Sniffer::new();
    sniffer.feed(response.body.as_bytes());
    out.usage = sniffer
        .finish()
        .map(|u| [u.input, u.output, u.cache_read, u.cache_write]);

    for line in response.body.lines() {
        let payload = line.strip_prefix("data: ").unwrap_or(line);
        let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
            continue;
        };
        collect(&v, &mut out);
    }
    // 非流式那一份整个是一个 JSON
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&response.body) {
        collect(&v, &mut out);
    }
    if response.status.is_some_and(|s| s >= 400) && out.error_kind.is_none() {
        out.error_kind = Some(format!("http_{}", response.status.unwrap_or(0)));
    }
    out
}

/// 从一个 JSON 值里捡出 stop_reason / 工具名 / 错误类型。
///
/// **三种方言的字段名都认**：Anthropic 的 `stop_reason`、OpenAI 的
/// `finish_reason`、Gemini 的 `finishReason`。它们的漂移正是这个测试集
/// 要防的东西。
fn collect(v: &serde_json::Value, out: &mut Extracted) {
    for key in ["stop_reason", "finish_reason", "finishReason"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str())
            && out.stop_reason.is_none()
        {
            out.stop_reason = Some(s.to_string());
        }
        // 流式的 delta 里也可能带
        if let Some(s) = v
            .get("delta")
            .and_then(|d| d.get(key))
            .and_then(|x| x.as_str())
            && out.stop_reason.is_none()
        {
            out.stop_reason = Some(s.to_string());
        }
        if let Some(c) = v.get("choices").and_then(|x| x.as_array())
            && let Some(s) = c
                .first()
                .and_then(|x| {
                    x.get(key)
                        .or_else(|| x.get("delta").and_then(|d| d.get(key)))
                })
                .and_then(|x| x.as_str())
            && out.stop_reason.is_none()
        {
            out.stop_reason = Some(s.to_string());
        }
    }
    // 工具调用：Anthropic 的 content_block_start / content 数组
    if let Some(cb) = v.get("content_block")
        && cb.get("type").and_then(|x| x.as_str()) == Some("tool_use")
        && let Some(n) = cb.get("name").and_then(|x| x.as_str())
    {
        out.tool_calls.push(n.to_string());
    }
    if let Some(items) = v.get("content").and_then(|x| x.as_array()) {
        for it in items {
            if it.get("type").and_then(|x| x.as_str()) == Some("tool_use")
                && let Some(n) = it.get("name").and_then(|x| x.as_str())
            {
                out.tool_calls.push(n.to_string());
            }
        }
    }
    // 错误
    if let Some(e) = v.get("error") {
        let kind = e
            .get("type")
            .and_then(|x| x.as_str())
            .unwrap_or("error")
            .to_string();
        out.error_kind.get_or_insert(kind);
    }
}

/// 录一个用例。**脱敏在这一步做，不是事后。**
///
/// `kinds` 传全部类别 —— 录制不该按某个 provider 的配置决定脱什么，
/// 它要脱的是「任何可能是凭据的东西」。
pub fn record(
    name: &str,
    note: &str,
    at_ms: u64,
    request: Recorded,
    response: Recorded,
) -> Fixture {
    let clean = |r: Recorded| -> Recorded {
        // 两道：先按凭据规则换成占位符（结构还在，值没了），再走一遍
        // 通用打码兜住规则没认出来的
        let redacted = tw_redact::redact::redact(&r.body, tw_redact::rules::Kind::all()).text;
        Recorded {
            body: tw_secret::mask_body(&redacted),
            ..r
        }
    };
    let request = clean(request);
    let response = clean(response);
    // **在脱敏之后提取。**回放时喂进去的也是脱敏后的那份，两边必须
    // 是同一个输入，否则第一次回放就对不上
    let expect = extract(&request, &response);
    Fixture {
        version: 1,
        name: name.to_string(),
        note: note.to_string(),
        recorded_at_ms: at_ms,
        request,
        response,
        expect,
    }
}

/// 回放一个用例，返回和录制时的差异。**空 = 没变。**
pub fn replay(f: &Fixture) -> Vec<String> {
    let got = extract(&f.request, &f.response);
    let mut diffs = Vec::new();
    macro_rules! cmp {
        ($field:ident, $label:expr) => {
            if got.$field != f.expect.$field {
                diffs.push(format!(
                    "{}：录制时为 {:?}，当前为 {:?}",
                    $label, f.expect.$field, got.$field
                ));
            }
        };
    }
    cmp!(model, "model");
    cmp!(input_tokens_estimate, "输入 token 估算");
    cmp!(cache, "带缓存");
    cmp!(tools, "带工具");
    cmp!(tool_count, "工具数");
    cmp!(image, "带图片");
    cmp!(thinking, "扩展思考");
    cmp!(stream, "流式");
    cmp!(usage, "用量四项");
    cmp!(stop_reason, "stop_reason");
    cmp!(tool_calls, "工具调用");
    cmp!(error_kind, "错误分类");
    diffs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(body: &str) -> Recorded {
        Recorded {
            path: "/v1/messages".into(),
            content_type: "application/json".into(),
            status: None,
            body: body.into(),
        }
    }
    fn resp(ct: &str, status: u16, body: &str) -> Recorded {
        Recorded {
            path: "/v1/messages".into(),
            content_type: ct.into(),
            status: Some(status),
            body: body.into(),
        }
    }

    const SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":120,\"cache_read_input_tokens\":40}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"Bash\"}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":30}}\n\n";

    #[test]
    fn a_recorded_case_replays_clean_right_after_recording() {
        // 录完立刻回放必须一致 —— 否则这个测试集从第一天起就是红的，
        // 而一个天天红的测试集等于没有。
        let f = record(
            "带工具调用的一轮",
            "",
            1,
            req(
                r#"{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"hi"}],"tools":[{"name":"Bash"}],"stream":true}"#,
            ),
            resp("text/event-stream", 200, SSE),
        );
        assert!(replay(&f).is_empty(), "{:?}", replay(&f));
    }

    #[test]
    fn the_four_token_dimensions_and_the_stop_reason_are_what_we_compare() {
        let f = record(
            "x",
            "",
            1,
            req(r#"{"model":"claude-sonnet-4-5","messages":[]}"#),
            resp("text/event-stream", 200, SSE),
        );
        assert_eq!(f.expect.usage, Some([120, 30, 40, 0]));
        assert_eq!(f.expect.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(f.expect.tool_calls, vec!["Bash".to_string()]);
    }

    #[test]
    fn a_drifting_upstream_shows_up_as_a_named_difference() {
        // **这是整个测试集存在的理由。**上游把 output_tokens 挪走了，
        // 或者改了 stop_reason 的写法 —— 单元测试全绿，这里会红。
        let mut f = record(
            "x",
            "",
            1,
            req(r#"{"model":"m","messages":[]}"#),
            resp("text/event-stream", 200, SSE),
        );
        // 装作上游改了：output 不再出现在 message_delta 里
        f.response.body = f
            .response
            .body
            .replace("\"usage\":{\"output_tokens\":30}", "\"usage\":{}");
        let d = replay(&f);
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].contains("用量四项"), "{d:?}");
        // **差异要说清「录的是什么、现在是什么」** —— 只说「不一致」
        // 的话，看的人还得自己去翻
        assert!(
            d[0].contains("录制时为") && d[0].contains("当前为"),
            "{}",
            d[0]
        );
    }

    #[test]
    fn the_openai_shape_is_recognised_too() {
        let body = "data: {\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n";
        let f = record(
            "x",
            "",
            1,
            req(r#"{"model":"gpt-5","messages":[]}"#),
            resp("text/event-stream", 200, body),
        );
        assert_eq!(f.expect.stop_reason.as_deref(), Some("stop"));
        assert_eq!(f.expect.usage, Some([10, 5, 0, 0]));
        assert!(replay(&f).is_empty());
    }

    #[test]
    fn an_error_response_records_which_bucket_it_lands_in() {
        // 一个 429 被塌成 502，客户端就不会退避了。错误映射
        // 变了是真事故。
        let f = record(
            "限流",
            "",
            1,
            req(r#"{"model":"m","messages":[]}"#),
            resp(
                "application/json",
                429,
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
            ),
        );
        assert_eq!(f.expect.error_kind.as_deref(), Some("rate_limit_error"));
        assert!(replay(&f).is_empty());
    }

    #[test]
    fn recording_strips_credentials_before_anything_is_written() {
        // **测试夹具会进 git。**一个装满真实密钥的 fixture 目录被 push
        // 上去，就再也收不回来了。
        let key = "sk-ant-api03-REALKEYAAAAAAAAAAAAAAAAAA";
        let f = record(
            "x",
            "",
            1,
            req(&format!(
                r#"{{"model":"m","messages":[{{"role":"user","content":"我的 key 是 {key}，还有 postgres://u:hunter2@h/db"}}]}}"#
            )),
            resp(
                "application/json",
                200,
                &format!(r#"{{"type":"message","echo":"{key}"}}"#),
            ),
        );
        let dump = serde_yaml_ng::to_string(&f).unwrap();
        assert!(!dump.contains("REALKEY"), "密钥进了夹具：\n{dump}");
        assert!(!dump.contains("hunter2"), "口令进了夹具：\n{dump}");
        // 但结构还在 —— 夹具的价值就在于它是一份真实形状
        assert!(dump.contains("postgres://u:"), "{dump}");
    }

    #[test]
    fn a_fixture_round_trips_through_yaml() {
        // 它要能进 git、能被人读、能被 CI 加载。
        let f = record(
            "x",
            "备注",
            1,
            req(r#"{"model":"m","messages":[]}"#),
            resp("application/json", 200, "{}"),
        );
        let text = serde_yaml_ng::to_string(&f).unwrap();
        let back: Fixture = serde_yaml_ng::from_str(&text).unwrap();
        assert_eq!(back.expect, f.expect);
        assert!(replay(&back).is_empty());
    }

    #[test]
    fn the_extractor_is_the_same_one_the_gateway_uses() {
        // **另写一份提取逻辑的话，回放验的是那一份，而线上跑的是另一
        // 份。**这条测试盯着「用的是同一个」这件事。
        let body = r#"{"model":"claude-opus-4-5","messages":[{"role":"user","content":[{"type":"image"}]}],"tools":[{"name":"a"},{"name":"b"}],"thinking":{"type":"enabled"}}"#;
        let f = record("x", "", 1, req(body), resp("application/json", 200, "{}"));
        let direct = tw_engine::RequestFacts::from_anthropic_body(
            &serde_json::from_str(&f.request.body).unwrap(),
        );
        assert_eq!(f.expect.model, direct.model);
        assert_eq!(f.expect.tool_count, direct.tool_count);
        assert_eq!(f.expect.image, direct.image);
        assert_eq!(f.expect.thinking, direct.thinking);
    }
}
