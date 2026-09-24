//! 上游探测。
//!
//! 分层。这里做 L1 和 L2 —— **两层都零成本**，不产生
//! 任何 token 消耗，所以可以随便跑。L3（真发一次推理）要花钱，必须先
//! 弹确认框，那是 M3 的事。

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tw_types::{Msg, msg};

/// 探测超时。比数据面的建连超时短 —— 探测是交互式的，用户在等着看结果，
/// 十秒的白屏比一条「连不上」难受得多。
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// 模型清单的结果。
///
/// **不能只用一个空 `Vec` 表达。**「上游没这个接口」「上游给了但我们没
/// 认出格式」「真的一个模型都没有」是三件不同的事，塌成一个空列表之后：
///
/// - UI 只能说「这家不提供模型列表」，而那在第二种情况下是**编的** ——
///   把我们自己的解析缺口说成了对方的特性；
/// - 模型清单从这里派生，静默为空之后用户不知道该去问谁。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelList {
    /// 拿到了
    Listed { models: Vec<String> },
    /// 上游没有这个接口。**不是错误**，很多中转站就是不实现 —— 但要说
    /// 出来，因为按模型路由、模型清单这些功能对它就用不了。
    NotImplemented { status: u16 },
    /// 上游返回了 2xx，但我们没认出它的形状。
    ///
    /// **这是我们的缺口，不是它的。** 必须和上一种分开报，否则每加一家
    /// 用新格式的上游，都会被我们说成「它不提供模型列表」，然后没人去
    /// 修解析器。
    Unrecognized { sample: String },
    /// 认出来了，但确实是空的
    Empty,
}

impl ModelList {
    pub fn models(&self) -> &[String] {
        match self {
            ModelList::Listed { models } => models,
            _ => &[],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResult {
    pub ok: bool,
    /// 从 base_url 猜出来的协议，猜不出是 None
    pub protocol: Option<String>,
    /// L1：建连到拿到响应头的耗时
    pub latency_ms: u64,
    /// L2 的结果，**带上为什么**
    pub models: ModelList,
    /// 失败时说清楚下一步做什么
    pub error: Option<Msg>,
}

impl ProbeResult {
    fn fail(latency_ms: u64, protocol: Option<String>, why: Msg) -> Self {
        Self {
            ok: false,
            protocol,
            latency_ms,
            models: ModelList::Empty,
            error: Some(why),
        }
    }
}

/// 验一个上游能不能用。
///
/// 判据是**「这把 key 能过认证吗」**，不是「有没有模型列表」。这两件事
/// 常被混在一起，而混了就会把「上游正常但不给列表」误报成「配错了」——
/// 那是最打击信心的一种误报，因为用户刚粘完 key。
pub async fn probe(
    http: &reqwest::Client,
    base_url: &str,
    headers: &[(String, String)],
    protocol: Option<tw_config::Protocol>,
) -> ProbeResult {
    let proto = protocol.or_else(|| tw_config::Provider::guess_protocol(base_url));
    let proto_name = proto.map(|p| format!("{p:?}"));
    let started = Instant::now();

    let chatgpt = proto == Some(tw_config::Protocol::Chatgpt);
    // Codex 后端的模型清单不在 `/v1/models`，而且必须带 `client_version`
    let url = if chatgpt {
        crate::chatgpt::models_url(base_url)
    } else {
        crate::forward::upstream_url(base_url, "/v1/models", None)
    };
    let mut req = http.get(&url).timeout(PROBE_TIMEOUT);
    req = crate::forward::apply_headers(req, headers);

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let ms = started.elapsed().as_millis() as u64;
            let why = if e.is_timeout() {
                msg!(
                    "gw.probe.timeout", secs = PROBE_TIMEOUT.as_secs() =>
                    "No answer within {secs} seconds. Check the endpoint address, or whether this \
                     upstream has to be reached through a proxy."
                )
            } else if e.is_connect() {
                msg!(
                    "gw.probe.connect" =>
                    "Could not connect. Check the spelling of the endpoint address and the network; \
                     if this upstream has to be reached through a proxy, configure the proxy first."
                )
            } else {
                // reqwest 的原话：系统或 TLS 库给的，没有别的说法
                msg!(
                    "gw.probe.request_failed", detail = e =>
                    "The request failed: {detail}"
                )
            };
            return ProbeResult::fail(ms, proto_name, why);
        }
    };

    let ms = started.elapsed().as_millis() as u64;
    let status = resp.status();

    // 401/403 是**唯一**能确定说「key 不对」的信号。其余非 2xx 都可能
    // 只是这家不实现 /v1/models —— 把 404 当成认证失败是常见的误判。
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return ProbeResult::fail(
            ms,
            proto_name,
            msg!(
                "gw.probe.key_rejected", status = status.as_u16() =>
                "The upstream rejected this key (HTTP {status}). Check the key for stray whitespace, \
                 and that it belongs to this upstream."
            ),
        );
    }

    let models = if status.is_success() {
        match resp.text().await {
            Ok(body) if chatgpt => classify_chatgpt_models(&body),
            Ok(body) => classify_models(&body),
            // 拿到了 2xx 但读 body 失败 —— 归到「没认出」而不是「不提供」，
            // 因为它确实有这个接口。
            Err(e) => ModelList::Unrecognized {
                sample: format!("the response body could not be read: {e}"),
            },
        }
    } else {
        // 非 2xx 但不是 401/403：认证这一关算过了，是它没这个接口。
        ModelList::NotImplemented {
            status: status.as_u16(),
        }
    };

    ProbeResult {
        ok: true,
        protocol: proto_name,
        latency_ms: ms,
        models,
        error: None,
    }
}

/// Codex 后端的模型清单：`{models:[{slug, visibility}]}`。后端隐藏的不列
fn classify_chatgpt_models(body: &str) -> ModelList {
    let parsed = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| crate::chatgpt::parse_models(&v));
    match parsed {
        Some(models) if models.is_empty() => ModelList::Empty,
        Some(models) => ModelList::Listed { models },
        None => ModelList::Unrecognized {
            sample: body.trim().chars().take(200).collect(),
        },
    }
}

/// 从各家的 /v1/models 响应里抠出模型名，**并说清楚认没认出来**。
///
/// 三种形状：OpenAI 的 `{data:[{id}]}`、Anthropic 的 `{data:[{id}]}`（同形）、
/// Gemini 的 `{models:[{name}]}`。认不出来返回 `Unrecognized` 而不是空
/// 列表 —— 一个编出来的模型列表比没有列表有害得多，而一句「这家不提供
/// 列表」在实际是我们没认出格式时同样是编的。
fn classify_models(body: &str) -> ModelList {
    // 采样只留大约 200 字节：够看出形状，又不至于把一整份响应（可能带
    // 敏感信息）塞进 UI 和日志。**按字符边界截断** —— 按字节切多字节
    // 字符会 panic，那是这个项目栽过两次的坑。
    let sample = || {
        let t = body.trim();
        let end = t
            .char_indices()
            .map(|(i, c)| i + c.len_utf8())
            .take_while(|&i| i <= 200)
            .last()
            .unwrap_or(0);
        t[..end].to_string()
    };

    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return ModelList::Unrecognized { sample: sample() };
    };
    let arr = v
        .get("data")
        .or_else(|| v.get("models"))
        .and_then(|x| x.as_array());
    let Some(arr) = arr else {
        return ModelList::Unrecognized { sample: sample() };
    };
    let models: Vec<String> = arr
        .iter()
        .filter_map(|m| {
            m.get("id")
                .or_else(|| m.get("name"))
                .and_then(|x| x.as_str())
                // Gemini 的 name 带 `models/` 前缀
                .map(|s| s.strip_prefix("models/").unwrap_or(s).to_string())
        })
        .collect();
    match (arr.is_empty(), models.is_empty()) {
        // 上游明确说「我一个模型都没有」。那是它的答案，不是我们的失败。
        (true, _) => ModelList::Empty,
        // 数组在、有元素、一个名字都抠不出来 —— 那是新格式，不是空。
        (false, true) => ModelList::Unrecognized { sample: sample() },
        (false, false) => ModelList::Listed { models },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_openai_and_anthropic_shape() {
        let b = r#"{"data":[{"id":"claude-sonnet-4-5"},{"id":"claude-opus-4-5"}]}"#;
        assert_eq!(
            classify_models(b).models(),
            ["claude-sonnet-4-5", "claude-opus-4-5"]
        );
    }

    #[test]
    fn reads_the_gemini_shape_and_strips_its_prefix() {
        let b = r#"{"models":[{"name":"models/gemini-2.5-pro"}]}"#;
        assert_eq!(classify_models(b).models(), ["gemini-2.5-pro"]);
    }

    #[test]
    fn an_unrecognised_shape_says_so_instead_of_looking_like_an_empty_list() {
        // 这是这段代码存在的核心理由：**「我们没认出格式」和「上游没这个
        // 接口」是两件事**。塌成一个空列表之后，UI 只能说「这家不提供模型
        // 列表」，而那在这种情况下是编的 —— 把我们自己的解析缺口说成了
        // 对方的特性，然后没人会去修解析器。
        for body in [
            r#"{"whatever":1}"#,
            "not json at all",
            r#"{"data":"not an array"}"#,
        ] {
            assert!(
                matches!(classify_models(body), ModelList::Unrecognized { .. }),
                "{body} 应该被判为没认出"
            );
        }
    }

    #[test]
    fn a_genuinely_empty_list_is_not_the_same_as_unrecognised() {
        // `data: []` 是上游明确说「我一个模型都没有」。
        assert!(matches!(
            classify_models(r#"{"data":[]}"#),
            ModelList::Empty
        ));
    }

    #[test]
    fn an_array_we_cannot_read_a_single_name_out_of_is_unrecognised() {
        // 数组在、有元素、一个名字都抠不出来 —— 那是新格式，不是空。
        assert!(matches!(
            classify_models(r#"{"data":[{"model_name":"x"}]}"#),
            ModelList::Unrecognized { .. }
        ));
    }

    #[test]
    fn the_sample_is_truncated_on_a_char_boundary() {
        // 采样会显示在 UI 和日志里，而按字节切多字节字符会 panic。
        let body = "响".repeat(500);
        match classify_models(&body) {
            ModelList::Unrecognized { sample } => assert!(sample.len() <= 200),
            other => panic!("应该是没认出，实际 {other:?}"),
        }
    }

    #[test]
    fn entries_without_a_usable_name_are_skipped_not_faked() {
        let b = r#"{"data":[{"id":"good"},{"object":"model"}]}"#;
        assert_eq!(classify_models(b).models(), ["good"]);
    }
}
