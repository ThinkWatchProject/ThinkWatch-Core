//! 请求重放（M6+）。
//!
//! 用途只有一个，但它是这个工具最常被需要的那一个：
//!
//! > 这条请求走中转慢/失败了。**同样一条**发给官方会怎么样？
//!
//! 「同样一条」是要害。手工复现一个 Claude Code 发出的请求几乎不可能 ——
//! 那是几十 KB 的 system prompt 加一堆工具定义，而任何一处不同都会让
//! 对比失去意义（说过，body 改一个字节就可能是缓存杀手）。我们手里
//! 正好有原样的那一份。
//!
//! # 三条纪律
//!
//! **一、它花钱。**和 L3 测速走同一套：先报价，用户点确认才发。
//!
//! **二、截断过的体不能重放。**存的时候超过 4 MB 会截断，而截断之后的
//! body 是**另一个请求** —— 拿它跑出来的结果去比对，比不跑更糟，因为
//! 用户会以为那是同一条。
//!
//! **三、脱敏照做。**重放走的是控制面，不经过数据面的管线，所以
//! 脱敏那一层要在这里显式调一次。少了它，一条本来会被脱敏的请求，
//! 会因为「重放」这个动作把密钥原样发给中转站。

use std::time::Instant;

use axum::{Json, extract::State, http::StatusCode};

use crate::ControlState;

type Fail = (StatusCode, String);

fn fail(code: StatusCode, e: impl std::fmt::Display) -> Fail {
    (code, e.to_string())
}

/// 找到那条请求，把**原样的**请求体取出来。
///
/// 注意不是 `request_detail` 里那份 —— 那一份是脱敏之后给人看的
/// （它会被复制进 issue）。重放要的是原样。
fn stored_body(
    g: &tw_store::Recorder,
    id: i64,
) -> Result<(tw_store::db::RequestRow, Vec<u8>), Fail> {
    let row = g
        .db()
        .get(id)
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("未找到第 {id} 号请求")))?;
    let raw = g
        .blobs()
        .get(row.at_ms, id, tw_store::Which::Request)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("第 {id} 号请求的请求体已不存在，可能已被清理"),
            )
        })?;
    let original = g
        .blobs()
        .original_len(row.at_ms, id, tw_store::Which::Request)
        .unwrap_or(raw.len());
    if original > raw.len() {
        // **截断之后的 body 是另一个请求。**拿它跑出来的结果去比对，
        // 比不跑更糟 —— 用户会以为那是同一条
        return Err((
            StatusCode::CONFLICT,
            format!(
                "第 {id} 号请求的请求体有 {original} 字节，仅保存了 {} 字节，无法原样重放",
                raw.len()
            ),
        ));
    }
    Ok((row, raw))
}

/// 报价。**不发任何请求。**
pub async fn quote(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::ReplayRequest>,
) -> Result<Json<tw_api::ReplayQuote>, Fail> {
    let store = s.store.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "请求记录未启动".to_string(),
        )
    })?;
    let (row, raw) = {
        let g = store.lock().await;
        stored_body(&g, req.id)?
    };
    let cfg = s.config();
    let provider = cfg
        .providers
        .iter()
        .find(|p| p.name == req.provider)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("未找到名为「{}」的上游", req.provider),
            )
        })?;

    // 输入 token 用记录里的真值 —— 那是上游报回来的，比任何估算都准。
    // 没有的话按字节粗估（和路由用的是同一个系数）
    let input = row.input_tokens.unwrap_or((raw.len() / 4) as i64).max(0) as u64;
    let output = row.output_tokens.unwrap_or(0).max(0) as u64;
    let book = s.gateway.pricing.load();
    let usage = tw_pricing::Usage {
        input,
        // 输出按上次那条的实际输出估。**它只是个估计**，模型这次可能
        // 说得更多或更少
        output: output.max(256),
        ..Default::default()
    };
    // **按要重放到的那个上游报价**，不是原来那条走的上游。计费方式和记账
    // 同一个口径：配置里写明了，或者最近一次响应里报过额度
    let quote = tw_gateway::quote::quote(
        &book,
        &provider.name,
        &row.model,
        &usage,
        s.gateway.billing_of(provider),
    );
    Ok(Json(tw_api::ReplayQuote {
        model: row.model.clone(),
        provider: provider.name.clone(),
        body_bytes: raw.len() as i64,
        input_tokens: input as i64,
        cost_micros: quote.cost_micros,
        billing: quote.billing.slug().to_string(),
        // 脱敏在重放里照做，但用户有权在按下去之前知道
        will_redact: !tw_gateway::guard::effective_kinds(provider, &tw_engine::Guard::default())
            .is_empty(),
        pricing_date: book.table().date.clone(),
    }))
}

/// 真的发。**这一步花钱。**
pub async fn run(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::ReplayRequest>,
) -> Result<Json<tw_api::ReplayResult>, Fail> {
    let store = s.store.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "请求记录未启动".to_string(),
        )
    })?;
    let (row, raw) = {
        let g = store.lock().await;
        stored_body(&g, req.id)?
    };
    let cfg = s.config();
    let provider = cfg
        .providers
        .iter()
        .find(|p| p.name == req.provider)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("未找到名为「{}」的上游", req.provider),
            )
        })?;
    // OAuth 要联网换 token。重放不经过数据面，但**凭据这一层
    // 必须走同一条路** —— 否则一个 OAuth 上游在重放里永远是「密钥取不到」
    let pk_http = s.gateway.client_for(&provider.name);
    let headers = s
        .gateway
        .headers_for(provider, &pk_http, None)
        .await
        .map_err(|e| {
            fail(
                StatusCode::BAD_REQUEST,
                format!("无法获取上游「{}」的凭据：{e}", provider.name),
            )
        })?;

    // **脱敏照做。**重放不经过数据面的管线，少了这一行，一条本来会被
    // 脱敏的请求会因为「重放」这个动作把密钥原样发给中转站
    let (body, ledger) = tw_gateway::guard::redact_outbound(
        cfg.security.redact,
        provider,
        &tw_engine::Guard::default(),
        bytes::Bytes::from(raw),
    );

    let url = tw_gateway::forward::upstream_url(&provider.base_url, &row.path, None);
    let http = s.http().clone();
    let started = Instant::now();
    let mut r = http.post(&url).header("content-type", "application/json");
    r = tw_gateway::forward::apply_headers(r, &headers);
    let resp = r.body(body).send().await.map_err(|e| {
        fail(
            StatusCode::BAD_GATEWAY,
            tw_gateway::forward::map_reqwest_error(e)
                .message()
                .to_string(),
        )
    })?;

    let status = resp.status().as_u16();
    let ttfb_ms = started.elapsed().as_millis() as i64;
    let text = resp.text().await.unwrap_or_default();
    let duration_ms = started.elapsed().as_millis() as i64;

    // 回显还原之后再脱敏给人看。**两步都要**：还原是为了让内容和原来
    // 那次可比，脱敏是因为这段文字会被复制进 issue
    let restored = tw_redact::redact::restore(&text, &ledger);
    Ok(Json(tw_api::ReplayResult {
        provider: provider.name.clone(),
        status,
        ttfb_ms,
        duration_ms,
        bytes: text.len() as i64,
        body: tw_secret::mask_body(&restored.chars().take(20_000).collect::<String>()),
        // 和原来那次并排比 —— 这是重放存在的理由
        original: tw_api::ReplayOriginal {
            provider: row.provider.clone(),
            status: row.status,
            ttfb_ms: row.ttfb_ms,
            duration_ms: row.duration_ms,
            bytes: row.bytes,
        },
    }))
}

/// 把一条真实请求导出成回放用例。
///
/// **「录制」不是一个新功能**：每一个请求和响应本来就在存储里，这一步
/// 只是把观测数据变成测试夹具。
///
/// 脱敏在 [`tw_gateway::fixture::record`] 里做，**不是事后**：夹具会进
/// git，一个装满真实密钥的目录被 push 上去就再也收不回来了。
pub async fn fixture(
    State(s): State<ControlState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<String, Fail> {
    let store = s.store.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "请求记录未启动".to_string(),
        )
    })?;
    let g = store.lock().await;
    let row = g
        .db()
        .get(id)
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("未找到第 {id} 号请求")))?;
    let body = |which| -> String {
        g.blobs()
            .get(row.at_ms, id, which)
            .map(|b| String::from_utf8_lossy(&b).to_string())
            .unwrap_or_default()
    };
    let req = body(tw_store::Which::Request);
    let resp = body(tw_store::Which::Response);
    if req.is_empty() && resp.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("第 {id} 号请求的响应体已不存在，可能已被清理"),
        ));
    }
    // 截断过的照样能当用例用 —— 它验的是「我们怎么理解这段字节」，
    // 而不是「原样重发一遍」（那是 /replay 的事，那边会拒绝截断的）
    let truncated = g
        .blobs()
        .original_len(row.at_ms, id, tw_store::Which::Response)
        .is_some_and(|o| o > resp.len());
    let note = format!(
        "录制自上游「{}」，时间戳 {}{}",
        row.provider,
        row.at_ms,
        if truncated {
            "。响应体在存储时已被截断，仅包含开头部分"
        } else {
            ""
        }
    );
    let f = tw_gateway::fixture::record(
        &format!("{}-{}", row.provider, row.model),
        &note,
        row.at_ms as u64,
        tw_gateway::fixture::Recorded {
            path: row.path.clone(),
            content_type: "application/json".into(),
            status: None,
            body: req,
        },
        tw_gateway::fixture::Recorded {
            path: row.path.clone(),
            content_type: if resp.starts_with("event:") || resp.contains("\ndata: ") {
                "text/event-stream".into()
            } else {
                "application/json".into()
            },
            status: row.status,
            body: resp,
        },
    );
    serde_yaml_ng::to_string(&f).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))
}
