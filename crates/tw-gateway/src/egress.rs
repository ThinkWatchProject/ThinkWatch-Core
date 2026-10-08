//! 发给上游的请求头：每种上游发什么。
//!
//! **白名单。**发出去的请求头只有三个来源：
//!
//! - 网关自己填的：User-Agent（ThinkWatch 的）、请求体经过转换时的 `content-type`
//!   （[`gateway_headers`]）；ChatGPT 账号另有它的身份头（[`crate::chatgpt::identity_headers`]）
//! - 上游配置里写的：凭据和用户加的头（`tw_config::Provider::outbound_headers`）
//! - 从客户端请求里按名取的：这种上游的协议要用的那几个（[`takes_from_client`]）
//!
//! 名单外的客户端请求头一个都不发。客户端是谁（`User-Agent`、`x-app`、`x-stainless-*`）、
//! 它的会话和安装 ID、浏览器带的 `Cookie` 和 `Origin`，都不是上游该知道的。黑名单只挡得住
//! 列出来的那些，客户端哪天多带一个头就原样漏给上游：ChatGPT 应用出具的证明
//! （`x-oai-attestation`）就是这样经网关漏给 OpenAI 的，令牌 20 秒后被吊销。
//!
//! 请求体同理。格式转换时由转换器按目标格式重新写，本来就只有请求的内容；同格式直通时
//! 原样发，只去掉客户端自动填的身份字段（[`strip_body_identity`]）。
//!
//! 有的上游按客户端放行：Kimi For Coding 只接编程工具的 User-Agent，百炼 Coding Plan
//! 拒绝通用的 User-Agent，有的中转站只放官方客户端。这样的上游打开
//! `forward_client_identity`，客户端自己的身份原样发过去（[`CLIENT_IDENTITY`]）。发的
//! 都是客户端的原值，不伪造；ChatGPT 账号不能打开。

use bytes::Bytes;
use http::HeaderMap;
use serde_json::Value;
use tw_config::Protocol;
use tw_dialect::ir::Dialect;

/// 这一跳发给谁、怎么发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hop {
    /// 这家说的格式。配置没写、地址也认不出时，按客户端的格式算（这时请求原样直通）
    pub protocol: Option<Protocol>,
    /// 请求体经过了格式转换：描述请求体的头由网关填
    pub translated: bool,
    /// 客户端的身份原样发给这家：它打开了 `forward_client_identity`，而且不是 ChatGPT 账号
    pub client_identity: bool,
    /// DeepSeek Harness 的请求发给 DeepSeek 官方：它自己的头照发（见 [`tw_dialect::harness`]）
    pub harness_to_deepseek: bool,
    /// DeepSeek Harness 的请求发给别家：`anthropic-beta` 不取客户端的原值，用去掉工具增删
    /// 那一项的版本（由发送处另外填）
    pub harness_elsewhere: bool,
}

impl Hop {
    /// 上游说什么格式：配置写明或按地址认出的；都没有就是客户端的格式
    pub fn protocol_for(provider: Option<Protocol>, client: Option<Dialect>) -> Option<Protocol> {
        provider.or(match client? {
            Dialect::Anthropic => Some(Protocol::Anthropic),
            Dialect::Chat => Some(Protocol::OpenaiChat),
            Dialect::Responses => Some(Protocol::OpenaiResponses),
            Dialect::Gemini => Some(Protocol::Gemini),
            // 客户端不说 Converse（`client_api` 认不出它的路径），走不到这里
            Dialect::Bedrock => Some(Protocol::Bedrock),
        })
    }
}

/// Codex 后端要从客户端取的头。
///
/// 照 openai/codex 发给 `/responses` 的头定，只留协议要的：
///
/// - `x-codex-turn-state`：同一轮里后端发的粘性路由令牌，这一轮后面的请求都要原样带回去
/// - `x-openai-internal-codex-responses-lite`：gpt-6 这类模型的请求体是精简格式，后端靠它认
/// - `x-codex-beta-features`：客户端打开的测试功能，后端按它决定回什么
///
/// 会话 ID 不在这里：它由 [`crate::chatgpt::identity_headers`] 从客户端的值取过来再填。
/// 窗口、线程、子代理这些 Codex 自己的上下文头不发，OpenCode 这类如实说明身份的客户端
/// 也不发它们
const CODEX_BACKEND: &[&str] = &[
    "x-codex-turn-state",
    "x-openai-internal-codex-responses-lite",
    "x-codex-beta-features",
];

/// OpenAI 接口（Chat、Responses）要从客户端取的头：重试不重复扣费的幂等键，和客户端给
/// 请求起的追踪 ID。`OpenAI-Organization`、`OpenAI-Project` 属于这家的密钥，写在上游配置里，
/// 不取客户端的 —— 客户端报的是它自己那边的组织，和这家的密钥对不上时是一个 401
const OPENAI: &[&str] = &["idempotency-key", "x-client-request-id"];

/// Anthropic 的这个协议头只对浏览器有意义（跨域放行），服务器之间转发没有用
const BROWSER_ACCESS: &str = "anthropic-dangerous-direct-browser-access";

/// 打开 `forward_client_identity` 时，另外原样发的客户端身份头（User-Agent 由
/// [`gateway_headers`] 发）。按上游实际核对的那些定：Kimi For Coding、百炼 Coding Plan
/// 看 User-Agent；只放官方客户端的中转站还看 Claude Code 的 `x-app`，以及 Codex 的
/// `originator`、`version`、会话 ID 和 `x-codex-*`
pub const CLIENT_IDENTITY: &[&str] =
    &["x-app", "originator", "version", "session_id", "session-id"];

/// [`CLIENT_IDENTITY`] 之外，按前缀认的那一族
const CLIENT_IDENTITY_PREFIX: &str = "x-codex-";

/// 客户端请求里的这个头发不发给这家。
pub fn takes_from_client(hop: &Hop, name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if hop.client_identity
        && (CLIENT_IDENTITY.contains(&n.as_str()) || n.starts_with(CLIENT_IDENTITY_PREFIX))
    {
        return true;
    }
    if hop.harness_to_deepseek && tw_dialect::harness::own_header(&n) {
        return true;
    }
    if hop.protocol == Some(Protocol::Chatgpt) {
        return CODEX_BACKEND.contains(&n.as_str());
    }
    // 请求体原样发时，说明它是什么的两个头照客户端的；转换过的由网关填
    if n == "content-type" || n == "accept" {
        return !hop.translated;
    }
    match hop.protocol {
        // `anthropic-*` 按前缀放行：Claude Code 的网关协议要求这一族原样转发，不按值挑
        Some(Protocol::Anthropic) => {
            n.starts_with("anthropic-")
                && n != BROWSER_ACCESS
                && !(hop.harness_elsewhere && n == "anthropic-beta")
        }
        Some(Protocol::OpenaiChat | Protocol::OpenaiResponses) => OPENAI.contains(&n.as_str()),
        // Bedrock 的请求一律是转换过的，头由网关和签名填
        Some(Protocol::Gemini | Protocol::Chatgpt | Protocol::Bedrock) | None => false,
    }
}

/// 网关自己填的头（ChatGPT 账号的身份头另见 [`crate::chatgpt::identity_headers`]）：
///
/// - User-Agent：ThinkWatch 的；这家打开了 `forward_client_identity` 时用客户端自己的
/// - `content-type`：请求体经过转换时一定是 JSON；原样发时照客户端的，客户端没写才填
pub fn gateway_headers(hop: &Hop, client: &HeaderMap) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if hop.protocol != Some(Protocol::Chatgpt) {
        let theirs = client
            .get(http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .filter(|_| hop.client_identity);
        let ua = theirs.map_or_else(crate::user_agent, str::to_string);
        out.push(("user-agent".to_string(), ua));
    }
    let chatgpt = hop.protocol == Some(Protocol::Chatgpt);
    if hop.translated || chatgpt || !client.contains_key(http::header::CONTENT_TYPE) {
        out.push(("content-type".to_string(), "application/json".to_string()));
    }
    out
}

/// 同格式直通时，请求体里客户端自动填的身份字段。
///
/// - Anthropic：`metadata.user_id`。Claude Code 在里面写设备 ID、账号 UUID 和会话 ID
/// - Responses：`client_metadata` 里 Codex 的安装 ID 和夹着它的 turn 元数据
///   （[`crate::chatgpt::IDENTITY_METADATA`]）；会话、线程这些照发
///
/// 返回去掉之后的请求体；没有可去的是 `None`，请求体一个字节都不动。这不是为了兼容去掉
/// 请求的内容，所以不算丢弃的字段。
///
/// **只剪掉那几个成员，别的字节一个不动**（见 [`cut_members`]）：解析再写回去的话，没开
/// `preserve_order` 的 serde_json 会把每个对象的键按字母重排 —— 工具定义、工具参数、整个
/// 请求都变了样（上游的提示缓存按字节认），而 Claude Code 的请求每一个都带着 `user_id`。
/// 剪不了的怪样子（同一个键写了两遍）才照旧解析、去掉、写回。
///
/// `body` 要是解得开的 JSON：调用方解过它（见管线的 `read`），这里不再解一遍
pub fn strip_body_identity(dialect: Dialect, body: &Bytes) -> Option<Bytes> {
    // 大多数请求没有这些字段：先便宜地看一眼，免得每个请求都把整个请求体走一遍
    let (parent, keys, drop_empty): (&str, &[&str], bool) = match dialect {
        Dialect::Anthropic => ("metadata", &["user_id"], true),
        Dialect::Responses => ("client_metadata", &crate::chatgpt::IDENTITY_METADATA, false),
        _ => return None,
    };
    let marks: &[&str] = match dialect {
        Dialect::Anthropic => &["\"user_id\""],
        _ => keys,
    };
    if !marks
        .iter()
        .any(|m| memchr::memmem::find(body, m.as_bytes()).is_some())
    {
        return None;
    }
    match cut_members(body, parent, keys, drop_empty) {
        Cut::Done(out) => Some(Bytes::from(out)),
        Cut::Nothing => None,
        Cut::Unusual => reserialize(dialect, body),
    }
}

/// 解析、去掉、写回：[`strip_body_identity`] 剪不了的时候走这条（键会按字母重排）。
fn reserialize(dialect: Dialect, body: &Bytes) -> Option<Bytes> {
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let obj = v.as_object_mut()?;
    let removed = match dialect {
        Dialect::Anthropic => {
            let meta = obj.get_mut("metadata").and_then(Value::as_object_mut);
            let removed = meta.is_some_and(|m| m.remove("user_id").is_some());
            // 只剩一个空对象的话整个去掉：`metadata` 本来就可以不写
            if obj
                .get("metadata")
                .and_then(Value::as_object)
                .is_some_and(serde_json::Map::is_empty)
            {
                obj.remove("metadata");
            }
            removed
        }
        _ => crate::chatgpt::remove_identity_metadata(obj),
    };
    if !removed {
        return None;
    }
    serde_json::to_vec(&v).ok().map(Bytes::from)
}

/// [`cut_members`] 的结果。
#[derive(Debug, PartialEq)]
enum Cut {
    /// 剪过的请求体
    Done(Vec<u8>),
    /// 没有要剪的
    Nothing,
    /// 同一个键写了两遍，或者不像 JSON 对象：剪的结果说不准和解析出来的一样，交给
    /// [`reserialize`]
    Unusual,
}

/// 顶层对象里 `parent` 那个对象，剪掉它的 `keys` 成员（连同分隔它的逗号）；剪空了而
/// `drop_empty` 的话，`parent` 整个剪掉。别的字节原样留着。
///
/// 只走顶层和 `parent` 这两层：别的值整个跳过，字符串里按字节找下一个引号或反斜杠。键里
/// 写了转义的（`"user\u005fid"`）照解析出来的样子比，和 serde_json 认的一样。
fn cut_members(body: &[u8], parent: &str, keys: &[&str], drop_empty: bool) -> Cut {
    let Some(top) = crate::splice::object(body, crate::splice::ws(body, 0)) else {
        return Cut::Unusual;
    };
    let mut parents = top.iter().filter(|m| m.is(body, parent));
    let (Some(p), None) = (parents.next(), parents.next()) else {
        // 一个都没有就没得剪；有两个的话 serde_json 留后一个，交给它
        return if top.iter().any(|m| m.is(body, parent)) {
            Cut::Unusual
        } else {
            Cut::Nothing
        };
    };
    if body[p.value.start] != b'{' {
        return Cut::Nothing;
    }
    let Some(inner) = crate::splice::object(body, p.value.start) else {
        return Cut::Unusual;
    };
    let gone: Vec<usize> = (0..inner.len())
        .filter(|&i| keys.iter().any(|k| inner[i].is(body, k)))
        .collect();
    if gone.is_empty() {
        return Cut::Nothing;
    }
    // 同一个键写了两遍
    if keys
        .iter()
        .any(|k| gone.iter().filter(|&&i| inner[i].is(body, k)).count() > 1)
    {
        return Cut::Unusual;
    }
    let cuts = if drop_empty && gone.len() == inner.len() {
        let at = top
            .iter()
            .position(|m| m.key.start == p.key.start)
            .unwrap_or_default();
        crate::splice::cuts(&top, &[at])
    } else {
        crate::splice::cuts(&inner, &gone)
    };
    let mut out = Vec::with_capacity(body.len());
    let mut at = 0;
    for c in cuts {
        out.extend_from_slice(&body[at..c.start]);
        at = c.end;
    }
    out.extend_from_slice(&body[at..]);
    Cut::Done(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hop(protocol: Protocol) -> Hop {
        Hop {
            protocol: Some(protocol),
            translated: false,
            client_identity: false,
            harness_to_deepseek: false,
            harness_elsewhere: false,
        }
    }

    /// Claude Code 实际会带的头
    const CLAUDE_CODE: &[&str] = &[
        "user-agent",
        "x-app",
        "x-stainless-lang",
        "x-stainless-os",
        "x-stainless-runtime-version",
        "x-stainless-timeout",
        "x-claude-code-session-id",
        "anthropic-dangerous-direct-browser-access",
    ];

    /// 浏览器和身份类的头，任何上游都不该从客户端那里拿到
    const NEVER: &[&str] = &[
        "cookie",
        "origin",
        "referer",
        "proxy-authorization",
        "authorization",
        "x-api-key",
        "x-goog-api-key",
        "host",
        "x-oai-attestation",
        "x-gemini-api-privileged-user-id",
        "x-goog-api-client",
        "x-thinkwatch-client",
    ];

    #[test]
    fn an_anthropic_upstream_gets_the_anthropic_headers_and_nothing_about_the_client() {
        let h = hop(Protocol::Anthropic);
        assert!(takes_from_client(&h, "anthropic-version"));
        assert!(takes_from_client(&h, "Anthropic-Beta"));
        assert!(takes_from_client(&h, "content-type"));
        for n in CLAUDE_CODE.iter().chain(NEVER) {
            assert!(!takes_from_client(&h, n), "{n}");
        }
    }

    #[test]
    fn an_openai_upstream_gets_only_the_idempotency_key_and_the_request_id() {
        for p in [Protocol::OpenaiChat, Protocol::OpenaiResponses] {
            let h = hop(p);
            assert!(takes_from_client(&h, "Idempotency-Key"));
            assert!(takes_from_client(&h, "x-client-request-id"));
            // 组织和项目属于这家的密钥，写在上游配置里
            assert!(!takes_from_client(&h, "openai-organization"));
            assert!(!takes_from_client(&h, "openai-project"));
            assert!(!takes_from_client(&h, "anthropic-beta"));
            for n in CLAUDE_CODE.iter().chain(NEVER) {
                assert!(!takes_from_client(&h, n), "{n}");
            }
        }
    }

    #[test]
    fn a_gemini_upstream_gets_nothing_from_the_client_but_the_body_type() {
        let h = hop(Protocol::Gemini);
        assert!(takes_from_client(&h, "content-type"));
        for n in [
            "x-goog-user-project",
            "x-server-timeout",
            "x-goog-api-client",
        ] {
            assert!(!takes_from_client(&h, n), "{n}");
        }
    }

    #[test]
    fn the_codex_backend_gets_its_protocol_headers_and_not_the_official_app() {
        let h = hop(Protocol::Chatgpt);
        for n in CODEX_BACKEND {
            assert!(takes_from_client(&h, n), "{n}");
        }
        for n in [
            "originator",
            "version",
            "session_id",
            "x-codex-installation-id",
            "x-codex-turn-metadata",
            "x-codex-window-id",
            // 描述请求体的头也由网关填：Codex 后端只认确切的 `application/json`
            "content-type",
            "accept",
        ]
        .iter()
        .chain(CLAUDE_CODE)
        .chain(NEVER)
        {
            assert!(!takes_from_client(&h, n), "{n}");
        }
    }

    #[test]
    fn a_translated_body_is_described_by_the_gateway() {
        let h = Hop {
            translated: true,
            ..hop(Protocol::OpenaiChat)
        };
        assert!(!takes_from_client(&h, "content-type"));
        assert!(!takes_from_client(&h, "accept"));
        let set = gateway_headers(&h, &HeaderMap::new());
        assert!(set.contains(&("content-type".into(), "application/json".into())));
    }

    #[test]
    fn the_client_identity_goes_only_where_it_was_asked_for() {
        let mut client = HeaderMap::new();
        client.insert(
            "user-agent",
            "claude-cli/2.1.1 (external, cli)".parse().unwrap(),
        );
        client.insert("content-type", "application/json".parse().unwrap());

        let plain = hop(Protocol::Anthropic);
        let ua = |set: Vec<(String, String)>| {
            set.into_iter()
                .find(|(k, _)| k == "user-agent")
                .map(|(_, v)| v)
                .unwrap()
        };
        assert!(ua(gateway_headers(&plain, &client)).starts_with("thinkwatch/"));
        assert!(!takes_from_client(&plain, "x-app"));

        let forwarding = Hop {
            client_identity: true,
            ..plain
        };
        assert_eq!(
            ua(gateway_headers(&forwarding, &client)),
            "claude-cli/2.1.1 (external, cli)"
        );
        for n in [
            "x-app",
            "originator",
            "version",
            "session_id",
            "x-codex-window-id",
        ] {
            assert!(takes_from_client(&forwarding, n), "{n}");
        }
        // 身份是客户端的，不是浏览器的，也不是凭据
        for n in NEVER.iter().filter(|n| !n.starts_with("x-codex-")) {
            assert!(!takes_from_client(&forwarding, n), "{n}");
        }
        // 客户端没报 User-Agent 就还是 ThinkWatch 的
        assert!(ua(gateway_headers(&forwarding, &HeaderMap::new())).starts_with("thinkwatch/"));
    }

    #[test]
    fn deepseek_harness_headers_go_only_to_deepseek() {
        let to_deepseek = Hop {
            harness_to_deepseek: true,
            ..hop(Protocol::Anthropic)
        };
        assert!(takes_from_client(
            &to_deepseek,
            "x-deepseek-harness-session"
        ));
        let elsewhere = Hop {
            harness_elsewhere: true,
            ..hop(Protocol::Anthropic)
        };
        assert!(!takes_from_client(&elsewhere, "x-deepseek-harness-session"));
        // 发给别家的 `anthropic-beta` 由发送处去掉工具增删那一项后另填
        assert!(!takes_from_client(&elsewhere, "anthropic-beta"));
        assert!(takes_from_client(&elsewhere, "anthropic-version"));
    }

    #[test]
    fn an_unknown_upstream_is_treated_as_the_client_format() {
        assert_eq!(
            Hop::protocol_for(None, Some(Dialect::Anthropic)),
            Some(Protocol::Anthropic)
        );
        assert_eq!(
            Hop::protocol_for(Some(Protocol::Gemini), Some(Dialect::Anthropic)),
            Some(Protocol::Gemini)
        );
        assert_eq!(
            Hop::protocol_for(None, Some(Dialect::Bedrock)),
            Some(Protocol::Bedrock)
        );
    }

    #[test]
    fn claude_code_user_id_is_not_sent() {
        let body = Bytes::from(
            serde_json::json!({
                "model": "claude-sonnet-5",
                "metadata": {"user_id": "user_abc_account_123_session_456"},
                "messages": [{"role": "user", "content": "hi"}]
            })
            .to_string(),
        );
        let out = strip_body_identity(Dialect::Anthropic, &body).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(v.get("metadata").is_none(), "{v}");
        assert_eq!(v["messages"][0]["content"], "hi");

        // 别的 metadata 字段留着
        let body = Bytes::from(r#"{"metadata":{"user_id":"u","note":"x"},"messages":[]}"#);
        let v: Value =
            serde_json::from_slice(&strip_body_identity(Dialect::Anthropic, &body).unwrap())
                .unwrap();
        assert_eq!(v["metadata"], serde_json::json!({"note": "x"}));

        // 没有身份字段：一个字节都不动
        let plain = Bytes::from(r#"{"messages":[{"role":"user","content":"hi"}]}"#);
        assert_eq!(strip_body_identity(Dialect::Anthropic, &plain), None);
        // 消息里提到 user_id 不算
        let mention = Bytes::from(r#"{"messages":[{"role":"user","content":"\"user_id\""}]}"#);
        assert_eq!(strip_body_identity(Dialect::Anthropic, &mention), None);
    }

    #[test]
    fn codex_installation_id_is_not_sent_but_the_session_is() {
        let body = Bytes::from(
            serde_json::json!({
                "model": "gpt-5.5",
                "client_metadata": {
                    "x-codex-installation-id": "inst-1",
                    "x-codex-turn-metadata": "{}",
                    "session_id": "conv-1"
                }
            })
            .to_string(),
        );
        let v: Value =
            serde_json::from_slice(&strip_body_identity(Dialect::Responses, &body).unwrap())
                .unwrap();
        assert_eq!(
            v["client_metadata"],
            serde_json::json!({"session_id": "conv-1"})
        );
        assert_eq!(strip_body_identity(Dialect::Chat, &body), None);
    }

    /// 去掉 `user_id` 之后，请求体别的字节一个不动：键的先后（工具定义、工具参数、整个请求）、
    /// 空白、转义都照客户端写的。以前解析再写回，每个对象的键都按字母重排了
    #[test]
    fn the_forwarded_body_is_the_client_body_less_the_identity() {
        let tools = r#""tools":[{"name":"Read","description":"读文件 \"quoted\" \\ path","input_schema":{"type":"object","properties":{"file_path":{"type":"string"},"offset":{"type":"number"},"limit":{"type":"number"}},"required":["file_path"],"additionalProperties":false,"$schema":"http://json-schema.org/draft-07/schema#"}}]"#;
        let messages = r#""messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"{\"user_id\":\"not this one\"} }]"}]},{"role":"assistant","content":[{"type":"tool_use","id":"toolu_02","name":"Read","input":{"zeta":1,"alpha":[2,{"b":3,"a":4}]}}]}]"#;
        let identity = r#""metadata":{"user_id":"user_abc_account_123_session_456"}"#;
        for (before, after) in [
            // Claude Code 的顺序：metadata 在工具定义后面
            (
                format!(
                    r#"{{"model":"claude-sonnet-5",{messages},"system":"s",{tools},{identity},"max_tokens":32000,"stream":true}}"#
                ),
                format!(
                    r#"{{"model":"claude-sonnet-5",{messages},"system":"s",{tools},"max_tokens":32000,"stream":true}}"#
                ),
            ),
            // 排在最前、最后
            (
                format!(r#"{{{identity},{messages}}}"#),
                format!(r#"{{{messages}}}"#),
            ),
            (
                format!(r#"{{{messages},{identity}}}"#),
                format!(r#"{{{messages}}}"#),
            ),
            // 带空白、换行的
            (
                format!("{{\n  {messages},\n  {identity} ,\n  \"stream\": true\n}}"),
                format!("{{\n  {messages},\n  \"stream\": true\n}}"),
            ),
            // metadata 里还有别的：只剪 user_id
            (
                r#"{"metadata":{"user_id":"u","note":"x"},"messages":[]}"#.to_string(),
                r#"{"metadata":{"note":"x"},"messages":[]}"#.to_string(),
            ),
            (
                r#"{"metadata": {"note": "x", "user_id": "u"}, "messages": []}"#.to_string(),
                r#"{"metadata": {"note": "x"}, "messages": []}"#.to_string(),
            ),
            (
                r#"{"metadata":{"a":1,"user_id":{"nested":["}"]},"b":2}}"#.to_string(),
                r#"{"metadata":{"a":1,"b":2}}"#.to_string(),
            ),
            // 键写成转义的：serde_json 认它是 user_id，这里也认（原文里别处得写着 `"user_id"`，
            // 先看的那一眼才放它过去，和以前一样）。反斜杠由 `char::from(92)` 拼：测试里直接
            // 写出来的转义，经过某些编辑工具会变成真字符
            (
                format!(
                    r#"{{"metadata":{{"user{b}u005fid":"u"}},"user_id":1}}"#,
                    b = char::from(92)
                ),
                r#"{"user_id":1}"#.to_string(),
            ),
        ] {
            let out = strip_body_identity(Dialect::Anthropic, &Bytes::from(before.clone()))
                .unwrap_or_else(|| panic!("nothing stripped from {before}"));
            assert_eq!(std::str::from_utf8(&out).unwrap(), after);
            // 和解析、去掉、写回的意思一样
            assert_eq!(
                serde_json::from_slice::<Value>(&out).unwrap(),
                serde_json::from_slice::<Value>(
                    &reserialize(Dialect::Anthropic, &Bytes::from(before)).unwrap()
                )
                .unwrap()
            );
        }

        let codex = r#"{"model":"gpt-5.5","input":[],"client_metadata":{"x-codex-turn-metadata":"{}","session_id":"conv-1","x-codex-installation-id":"inst-1"},"stream":true}"#;
        let out = strip_body_identity(Dialect::Responses, &Bytes::from(codex)).unwrap();
        assert_eq!(
            std::str::from_utf8(&out).unwrap(),
            r#"{"model":"gpt-5.5","input":[],"client_metadata":{"session_id":"conv-1"},"stream":true}"#
        );
        // 剪空了也留着这个对象，和以前一样
        let codex = r#"{"client_metadata":{"x-codex-installation-id":"inst-1","x-codex-turn-metadata":"{}"}}"#;
        let out = strip_body_identity(Dialect::Responses, &Bytes::from(codex)).unwrap();
        assert_eq!(
            std::str::from_utf8(&out).unwrap(),
            r#"{"client_metadata":{}}"#
        );
    }

    /// 同一个键写了两遍：剪的结果说不准和 serde_json 认的一样（它留后一个），照旧解析、
    /// 去掉、写回
    #[test]
    fn a_key_written_twice_falls_back_to_reserializing() {
        for body in [
            r#"{"metadata":{"user_id":"a"},"x":1,"metadata":{"user_id":"b","n":1}}"#,
            r#"{"metadata":{"user_id":"a","user_id":"b"},"x":1}"#,
        ] {
            let body = Bytes::from(body);
            assert_eq!(
                strip_body_identity(Dialect::Anthropic, &body),
                reserialize(Dialect::Anthropic, &body)
            );
        }
        assert_eq!(
            cut_members(
                br#"{"metadata":1,"metadata":{"user_id":"b"}}"#,
                "metadata",
                &["user_id"],
                true
            ),
            Cut::Unusual
        );
    }

    /// 随机拼出来的请求：剪出来的和解析、去掉、写回的意思一样，剪没剪也一样；剪过的那一份
    /// 只少了几段，别的字节按原样、原来的先后都在
    #[test]
    fn cutting_means_what_reserializing_meant() {
        struct Rng(u64);
        impl Rng {
            fn below(&mut self, n: usize) -> usize {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                (self.0 % n as u64) as usize
            }
            fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
                xs[self.below(xs.len())]
            }
        }
        let mut r = Rng(0x51_7cc1_b727_220a);
        const KEYS: &[&str] = &[
            "\"user_id\"",
            "\"user\\u005fid\"",
            "\"note\"",
            "\"a\"",
            "\"x-codex-installation-id\"",
            "\"x-codex-turn-metadata\"",
            "\"session_id\"",
        ];
        const VALUES: &[&str] = &[
            "1",
            "-2.5e3",
            "true",
            "null",
            "\"s\"",
            "\"q\\\"}]{[,\\\\\"",
            "\"\\\"user_id\\\"\"",
            "[]",
            "{}",
            "[1,{\"user_id\":\"deep\"},\"]\"]",
            "{\"metadata\":{\"user_id\":\"deep\"}}",
        ];
        const WS: &[&str] = &["", " ", "\n  ", "\t"];
        let inner = |r: &mut Rng| {
            let members: Vec<String> = (0..r.below(4))
                .map(|_| {
                    let (k, a, b, v) = (r.pick(KEYS), r.pick(WS), r.pick(WS), r.pick(VALUES));
                    format!("{k}{a}:{b}{v}")
                })
                .collect();
            format!("{{{}}}", members.join(","))
        };
        for _ in 0..5_000 {
            let mut top = Vec::new();
            for _ in 0..r.below(5) {
                let (k, v) = match r.below(4) {
                    0 => ("\"metadata\"", inner(&mut r)),
                    1 => ("\"client_metadata\"", inner(&mut r)),
                    2 => ("\"messages\"", r.pick(VALUES).to_string()),
                    _ => (r.pick(KEYS), r.pick(VALUES).to_string()),
                };
                let (a, b, c, d) = (r.pick(WS), r.pick(WS), r.pick(WS), r.pick(WS));
                top.push(format!("{a}{k}{b}:{c}{v}{d}"));
            }
            let (a, b) = (r.pick(WS), r.pick(WS));
            let body = Bytes::from(format!("{a}{{{}}}{b}", top.join(",")));
            if serde_json::from_slice::<Value>(&body).is_err() {
                continue;
            }
            for d in [Dialect::Anthropic, Dialect::Responses] {
                let cut = strip_body_identity(d, &body);
                let old = reserialize(d, &body);
                // 旧的写法先看一眼原文里有没有那几个字：没有就不解析
                let marked = match d {
                    Dialect::Anthropic => memchr::memmem::find(&body, b"\"user_id\"").is_some(),
                    _ => crate::chatgpt::IDENTITY_METADATA
                        .iter()
                        .any(|m| memchr::memmem::find(&body, m.as_bytes()).is_some()),
                };
                let old = old.filter(|_| marked);
                let value = |b: &Option<Bytes>| {
                    b.as_ref()
                        .map(|b| serde_json::from_slice::<Value>(b).unwrap())
                };
                assert_eq!(
                    value(&cut),
                    value(&old),
                    "{d:?} {}",
                    String::from_utf8_lossy(&body)
                );
                // 剪过的那一份是原文的一个子序列：别的字节原样、按原来的先后
                if let Some(cut) = &cut
                    && cut_members(&body, "metadata", &["user_id"], true) != Cut::Unusual
                    && cut_members(
                        &body,
                        "client_metadata",
                        &crate::chatgpt::IDENTITY_METADATA,
                        false,
                    ) != Cut::Unusual
                {
                    let mut rest = body.iter();
                    assert!(
                        cut.iter().all(|c| rest.any(|b| b == c)),
                        "{}",
                        String::from_utf8_lossy(&body)
                    );
                }
            }
        }
    }
}
