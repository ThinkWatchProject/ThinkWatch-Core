//! 列模型、查单个模型。

use axum::extract::{OriginalUri, RawQuery, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};

use super::now_ms;
use crate::error::GatewayError;
use crate::state::AppState;
use tw_types::msg;

/// 列模型、查单个模型时，客户端是哪一种。
///
/// 这两个请求没有体，路径也分不出 Anthropic 和 OpenAI（都是 `/v1/models`），
/// 只能看请求头：
///
/// - `/v1beta` 路径、或者把密钥放在 Google 位置上的是 Gemini
/// - 带 `anthropic-version`、或者把密钥放在 `x-api-key` 的是 Anthropic ——
///   两个信号**任一个**就算：Claude Code 用 `ANTHROPIC_AUTH_TOKEN` 时密钥走
///   Bearer，但版本头照带；手写的脚本常常只放 `x-api-key`
/// - 其余按 OpenAI 算
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListingShape {
    Anthropic,
    Openai,
    Gemini,
}

impl ListingShape {
    fn of(path: &str, headers: &HeaderMap, position: crate::auth::KeyPosition) -> Self {
        if path.starts_with("/v1beta") || position == crate::auth::KeyPosition::GoogleHeader {
            ListingShape::Gemini
        } else if headers.contains_key("anthropic-version")
            || position == crate::auth::KeyPosition::AnthropicHeader
        {
            ListingShape::Anthropic
        } else {
            ListingShape::Openai
        }
    }

    /// 这种客户端能用哪些协议的上游的模型。和请求那条路用同一张表：列出来的是
    /// 用来生成回答的模型，四种格式互相转换，所以谁都能用
    fn protocols(&self) -> Vec<&'static str> {
        use crate::client_api::{ClientApi, slugs};
        let api = match self {
            ListingShape::Anthropic => ClientApi::AnthropicMessages,
            ListingShape::Openai => ClientApi::OpenaiChat,
            ListingShape::Gemini => ClientApi::Gemini,
        };
        slugs(api.servable_by(true))
    }
}

/// `GET /v1/models`。
///
/// 三种方言的响应结构不同，但**列表内容来自同一个函数** —— 差别只在
/// 外壳。
pub(super) async fn list_models(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    OriginalUri(uri): OriginalUri,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Result<Response, GatewayError> {
    let rt = state.runtime();
    if !rt.allow.allows(peer.ip()) {
        return Err(GatewayError::auth(msg!(
            "gw.auth.source_not_allowed", peer = peer.ip() =>
            "{peer} is not among the allowed source addresses."
        )));
    }
    let (client, position) = state.identify(&headers, query.as_deref())?;
    let allow = rt
        .config
        .clients
        .iter()
        .find(|c| c.name == client)
        .and_then(|c| c.allow.clone());
    let shape = ListingShape::of(uri.path(), &headers, position);
    let models = state
        .catalog
        .load()
        .resolve_allowed(Some(&shape.protocols()), allow.as_deref());

    let now = now_ms() / 1000;
    let body = match shape {
        ListingShape::Gemini => serde_json::json!({
            "models": models.iter().map(|m| serde_json::json!({
                "name": format!("models/{m}"),
            })).collect::<Vec<_>>()
        }),
        ListingShape::Anthropic => serde_json::json!({
            "object": "list",
            "data": models.iter().map(|m| anthropic_model(m, now)).collect::<Vec<_>>(),
            "has_more": false,
            "first_id": models.first(),
            "last_id": models.last(),
        }),
        ListingShape::Openai => serde_json::json!({
            "object": "list",
            "data": models.iter().map(|m| serde_json::json!({
                "id": m, "object": "model", "created": now,
            })).collect::<Vec<_>>()
        }),
    };
    Ok(axum::Json(body).into_response())
}

/// Anthropic 格式的一个模型对象。
///
/// **是 OpenAI 那个对象的超集**：Anthropic 的字段（`type`、`display_name`、
/// `created_at`）之外，`object` 和 `created` 照样在。只放 `x-api-key` 的客户端也被
/// 认成 Anthropic，其中有按 OpenAI 的形状读列表的，不能让它们读不出来。
///
/// Claude 的模型再带上 `anthropic_family_tier`：Claude Desktop 按它把模型归到
/// opus / sonnet / haiku，配置里写的 `sonnet` 这样的简称靠它解析。**只看模型名**，
/// 名字里看不出是 Claude 的一律不标 —— 把别家的模型标成 Claude 是在替客户端撒谎。
fn anthropic_model(id: &str, now: u64) -> serde_json::Value {
    let created_at = chrono::DateTime::from_timestamp(now as i64, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut m = serde_json::json!({
        "type": "model",
        "id": id,
        "display_name": display_name(id).unwrap_or_else(|| id.to_string()),
        "created_at": created_at,
        "object": "model",
        "created": now,
    });
    if let Some(tier) = family_tier(id) {
        m["anthropic_family_tier"] = tier.into();
    }
    m
}

/// Claude 模型的名字：`claude-sonnet-4-5-20250929` → `Claude Sonnet 4.5`。
///
/// 只认最后一段（`/` 之后）以 `claude-` 开头、每一节都是字母或数字的；日期那一节
/// 去掉，相邻的数字用点连起来。**认不出就是 `None`**，调用方用模型 ID 本身 ——
/// 客户端看到和 ID 一样的名字时会自己想办法，一个猜错的名字它却会照着显示。
fn display_name(id: &str) -> Option<String> {
    let last = id.rsplit('/').next()?;
    let rest = last.strip_prefix("claude-")?;
    let mut words: Vec<String> = vec!["Claude".into()];
    let mut number = false;
    for part in rest.split('-') {
        if part.is_empty() {
            return None;
        }
        if part.len() == 8 && part.bytes().all(|b| b.is_ascii_digit()) {
            // 发布日期，不是名字的一部分
            continue;
        }
        if part.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
            match words.last_mut() {
                Some(w) if number => {
                    w.push('.');
                    w.push_str(part);
                }
                _ => words.push(part.to_string()),
            }
            number = true;
        } else if part.bytes().all(|b| b.is_ascii_alphanumeric()) {
            let mut c = part.chars();
            let first = c.next()?.to_ascii_uppercase();
            words.push(std::iter::once(first).chain(c).collect());
            number = false;
        } else {
            return None;
        }
    }
    (words.len() > 1).then(|| words.join(" "))
}

/// 名字里看得出是哪一档的 Claude 模型：`opus`、`sonnet` 或 `haiku`。
fn family_tier(id: &str) -> Option<&'static str> {
    let lower = id.to_ascii_lowercase();
    if !lower.contains("claude") && !lower.contains("anthropic") {
        return None;
    }
    let mut tiers = ["opus", "sonnet", "haiku"]
        .into_iter()
        .filter(|t| lower.contains(t));
    // 名字里同时出现两档的（一个路由别名）不猜
    match (tiers.next(), tiers.next()) {
        (Some(t), None) => Some(t),
        _ => None,
    }
}

/// `GET /v1/models/:model`。
///
/// **不许可就当它不存在（404），不是 403。**回 403 等于告诉对方
/// 「这个模型在，只是你不能用」—— 而列表里根本没列它，两处说法不一致
/// 本身就是一条信息。
pub(super) async fn get_model(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    axum::extract::Path(model): axum::extract::Path<String>,
    OriginalUri(uri): OriginalUri,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Result<Response, GatewayError> {
    let rt = state.runtime();
    if !rt.allow.allows(peer.ip()) {
        return Err(GatewayError::auth(msg!(
            "gw.auth.source_not_allowed", peer = peer.ip() =>
            "{peer} is not among the allowed source addresses."
        )));
    }
    let (client, position) = state.identify(&headers, query.as_deref())?;
    let allow = rt
        .config
        .clients
        .iter()
        .find(|c| c.name == client)
        .and_then(|c| c.allow.clone());
    let catalog = state.catalog.load();
    // 目录空着时不拦 —— 那说明探测还没回来或者上游都不给列表，这时候
    // 拦等于把整个网关关掉（和请求那条路同一个判断）
    let shape = ListingShape::of(uri.path(), &headers, position);
    if !catalog.is_empty() && !catalog.admits(&model, Some(&shape.protocols()), allow.as_deref()) {
        return Err(GatewayError::new(
            crate::error::Source::Request,
            msg!(
                "gw.model.unknown", model = model.clone() =>
                "There is no model {model}. GET /v1/models lists the models that are available."
            ),
        ));
    }
    let now = now_ms() / 1000;
    let body = match shape {
        ListingShape::Gemini => {
            serde_json::json!({ "name": format!("models/{model}") })
        }
        ListingShape::Anthropic => anthropic_model(&model, now),
        ListingShape::Openai => {
            serde_json::json!({ "id": model, "object": "model", "created": now })
        }
    };
    Ok(axum::Json(body).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_model_ids_get_their_names() {
        for (id, name) in [
            ("claude-sonnet-4-5", "Claude Sonnet 4.5"),
            ("claude-sonnet-4-5-20250929", "Claude Sonnet 4.5"),
            ("claude-opus-4-1-20250805", "Claude Opus 4.1"),
            ("claude-3-5-haiku-20241022", "Claude 3.5 Haiku"),
            ("claude-fable-5-1", "Claude Fable 5.1"),
            ("anthropic/claude-sonnet-4.5", "Claude Sonnet 4.5"),
        ] {
            assert_eq!(display_name(id).as_deref(), Some(name), "{id}");
        }
    }

    #[test]
    fn a_name_that_cannot_be_read_is_not_guessed() {
        for id in [
            "deepseek-chat",
            "gpt-5",
            "claude-",
            "claude--x",
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        ] {
            assert_eq!(display_name(id), None, "{id}");
        }
    }

    #[test]
    fn only_claude_models_are_given_a_family_tier() {
        assert_eq!(family_tier("claude-opus-4-1"), Some("opus"));
        assert_eq!(family_tier("claude-3-5-haiku-20241022"), Some("haiku"));
        assert_eq!(
            family_tier("us.anthropic.claude-sonnet-4-5-20250929-v1:0"),
            Some("sonnet")
        );
        assert_eq!(family_tier("claude-fable-5-1"), None);
        // 名字里有 sonnet，但看不出是 Claude
        assert_eq!(family_tier("my-sonnet-alias"), None);
        assert_eq!(family_tier("claude-opus-or-sonnet"), None);
    }
}
