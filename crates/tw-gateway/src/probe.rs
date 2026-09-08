//! 上游探测。
//!
//! 分层见 DESIGN.md §4.6。这里做 L1 和 L2 —— **两层都零成本**，不产生
//! 任何 token 消耗，所以可以随便跑。L3（真发一次推理）要花钱，必须先
//! 弹确认框，那是 M3 的事。

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// 探测超时。比数据面的建连超时短 —— 探测是交互式的，用户在等着看结果，
/// 十秒的白屏比一条「连不上」难受得多。
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResult {
    pub ok: bool,
    /// 从 base_url 猜出来的协议，猜不出是 None
    pub protocol: Option<String>,
    /// L1：建连到拿到响应头的耗时
    pub latency_ms: u64,
    /// L2：上游报出来的模型。空表示这个上游不给列表 —— **不是失败**，
    /// 很多中转站就是不实现 /v1/models
    pub models: Vec<String>,
    /// 失败时说清楚下一步做什么
    pub error: Option<String>,
}

impl ProbeResult {
    fn fail(latency_ms: u64, protocol: Option<String>, msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            protocol,
            latency_ms,
            models: Vec::new(),
            error: Some(msg.into()),
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
    key: &str,
    protocol: Option<tw_config::Protocol>,
) -> ProbeResult {
    let proto = protocol.or_else(|| tw_config::Provider::guess_protocol(base_url));
    let proto_name = proto.map(|p| format!("{p:?}"));
    let started = Instant::now();

    let url = crate::forward::upstream_url(base_url, "/v1/models", None);
    let mut req = http.get(&url).timeout(PROBE_TIMEOUT);
    req = crate::forward::apply_credential(req, proto, key);

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let ms = started.elapsed().as_millis() as u64;
            let msg = if e.is_timeout() {
                format!("{PROBE_TIMEOUT:?} 内没有响应。检查 base_url，或者这家上游是不是需要代理。")
            } else if e.is_connect() {
                "连不上。检查 base_url 拼写和网络；如果这家上游要走代理，先配好代理。".to_string()
            } else {
                format!("请求失败：{e}")
            };
            return ProbeResult::fail(ms, proto_name, msg);
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
            format!(
                "上游拒绝了这把密钥（HTTP {}）。检查 key 有没有多余的空格，以及它是不是这家的。",
                status.as_u16()
            ),
        );
    }

    let models = if status.is_success() {
        resp.text()
            .await
            .ok()
            .map(|t| extract_models(&t))
            .unwrap_or_default()
    } else {
        // 非 2xx 但不是 401/403：认证这一关算过了，列表拿不到而已。
        Vec::new()
    };

    ProbeResult {
        ok: true,
        protocol: proto_name,
        latency_ms: ms,
        models,
        error: None,
    }
}

/// 从各家的 /v1/models 响应里抠出模型名。
///
/// 三种形状：OpenAI 的 `{data:[{id}]}`、Anthropic 的 `{data:[{id}]}`（同形）、
/// Gemini 的 `{models:[{name}]}`。**认不出来就返回空**，不猜 —— 一个编
/// 出来的模型列表比没有列表有害得多。
fn extract_models(body: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let arr = v
        .get("data")
        .or_else(|| v.get("models"))
        .and_then(|x| x.as_array());
    let Some(arr) = arr else { return Vec::new() };
    arr.iter()
        .filter_map(|m| {
            m.get("id")
                .or_else(|| m.get("name"))
                .and_then(|x| x.as_str())
                // Gemini 的 name 带 `models/` 前缀
                .map(|s| s.strip_prefix("models/").unwrap_or(s).to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_openai_and_anthropic_shape() {
        let b = r#"{"data":[{"id":"claude-sonnet-4-5"},{"id":"claude-opus-4-5"}]}"#;
        assert_eq!(
            extract_models(b),
            vec!["claude-sonnet-4-5", "claude-opus-4-5"]
        );
    }

    #[test]
    fn reads_the_gemini_shape_and_strips_its_prefix() {
        let b = r#"{"models":[{"name":"models/gemini-2.5-pro"}]}"#;
        assert_eq!(extract_models(b), vec!["gemini-2.5-pro"]);
    }

    #[test]
    fn an_unrecognised_shape_yields_nothing_rather_than_a_guess() {
        // 编出来的模型列表比没有列表有害得多 —— 用户会照着它去配路由。
        assert!(extract_models(r#"{"whatever":1}"#).is_empty());
        assert!(extract_models("not json at all").is_empty());
        assert!(extract_models(r#"{"data":"not an array"}"#).is_empty());
    }

    #[test]
    fn entries_without_a_usable_name_are_skipped_not_faked() {
        let b = r#"{"data":[{"id":"good"},{"object":"model"}]}"#;
        assert_eq!(extract_models(b), vec!["good"]);
    }
}
