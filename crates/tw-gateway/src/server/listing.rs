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
        // Anthropic 和 OpenAI 的 /v1/models 形状一样
        _ => serde_json::json!({
            "object": "list",
            "data": models.iter().map(|m| serde_json::json!({
                "id": m, "object": "model", "created": now,
            })).collect::<Vec<_>>()
        }),
    };
    Ok(axum::Json(body).into_response())
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
        _ => serde_json::json!({ "id": model, "object": "model", "created": now }),
    };
    Ok(axum::Json(body).into_response())
}
