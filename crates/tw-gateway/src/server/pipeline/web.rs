//! Hosted web operations keep the normal model pipeline for every model round.
//! The outer operation records zero model usage; each actual model round is metered
//! by its own normal request record, so tool loops do not hide or double bill tokens.

use std::sync::Arc;

mod socket;
pub(in crate::server) use socket::serve_socket;

use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::{Value, json};
use tw_dialect::ir::Dialect;

use super::{Chosen, Inbound};
use crate::web_tools::{
    self,
    protocol::{self, Call, Plan},
};
use crate::{
    GatewayError,
    state::{AppState, Runtime},
};

fn failed(message: tw_types::Msg) -> GatewayError {
    GatewayError::new(crate::error::Source::Request, message)
}

pub(super) async fn run(
    state: AppState,
    rt: Arc<Runtime>,
    mut req: Inbound,
    live: crate::live::Pass,
    plan: Plan,
    original: Value,
) -> Result<Response, GatewayError> {
    let mut ending = None;
    let mut trace = Vec::new();
    let result = async {
        let (mut reading, fp) = super::read(
            &req,
            "web_search".into(),
            Some(&original),
            (state.local_time)(),
        );
        req.web_parent = plan.standalone(&original).is_none();
        // Local execution may work without any upstream. Explicit routing denials
        // still apply, as do key limits, screening, redaction and cancellation.
        if let Ok(tw_engine::Outcome::Deny { reason, .. }) = rt.engine.route(&reading.facts) {
            return Err(GatewayError::denied(web_tools::error(reason)));
        }
        // The local parent has no billable model input.
        reading.facts.input_tokens = 0;
        let choice = crate::server::Choice {
            route: rt.engine.route_of(&req.client_name).into(),
            ..Default::default()
        };
        let decision = tw_engine::Decision {
            candidates: Vec::new(),
            matched_rule: String::new(),
            via_group: None,
            set: Default::default(),
            rewritten_by: Vec::new(),
            pinned: Vec::new(),
        };
        let chosen = Chosen {
            parsed: Some(original.clone()),
            reading,
            fp,
            conv: None,
            choice,
            decision,
        };
        let tail = super::enter(&state, &rt, &mut req, chosen, live, &mut ending).await?;
        let id = tail.started.id;
        ending
            .as_mut()
            .expect("entered request")
            .add_usage(&tw_dialect::ir::Usage::default());
        state.bus.emit(crate::server::routed_nowhere(
            id,
            tail.started.choice.clone(),
        ));
        let screened: Value = serde_json::from_slice(&req.body)
            .map_err(|_| failed(web_tools::error("invalid screened request")))?;
        let stream = original["stream"] == true;
        let standalone = plan.standalone(&screened);
        let body = if let Some(request) = standalone {
            let result = tokio::select! {
                _ = req.abort.wait() => return Err(GatewayError::aborted()),
                result = web_tools::execute_scoped(&rt, &req.client_name, &request, &plan.allowed, &plan.blocked) => result,
            };
            match result {
                Ok(result) => {
                    trace.push(trace_of(&request, &result));
                    let call = Call {
                        native_id: uuid::Uuid::new_v4().simple().to_string(),
                        id: uuid::Uuid::new_v4().simple().to_string(),
                        request,
                    };
                    rt.web_tools
                        .history
                        .remember(&req.client_name, req.dialect, &call, &result);
                    let mut answer = protocol::standalone_response(
                        original["model"].as_str().unwrap_or(""), &call, &result,
                    );
                    if original["safeguards"].as_array().is_some_and(|items| items.iter().any(|v| v["type"] == "dangerous_tool_use")) {
                        // There are no client-executed tool_use blocks to classify.
                        answer["safeguard_results"] = json!([{"type":"dangerous_tool_use","status":{"type":"available","tool_uses":{}}}]);
                    }
                    answer
                }
                Err(_) if rt.config.web_tools.fallback_to_upstream => {
                    drop(tail.passes.1);
                    let mut response = forward_original(&state, &rt, &req, &original).await?;
                    response.headers_mut().insert(
                        "x-thinkwatch-web-tools",
                        axum::http::HeaderValue::from_static("upstream"),
                    );
                    response.headers_mut().insert(
                        "x-thinkwatch-web-fallback",
                        axum::http::HeaderValue::from_static("1"),
                    );
                    return Ok(response);
                }
                Err(e) => return Err(failed(e)),
            }
        } else {
            // Model rounds acquire the same key's concurrency slot. Release the
            // outer slot first, including when the key only allows one request.
            drop(tail.passes.1);
            match model_loop(&state, &rt, &req, &plan, screened, &mut trace).await? {
                ModelResult::Answer(body) => body,
                ModelResult::Failed(response) => return Ok(response),
            }
        };
        let body = if let Some(filter) = crate::guard::AnswerFilter::of(&rt) {
            let (changed, hits) = tw_guard::content::answer::whole(
                &filter.answer,
                filter.acts,
                req.dialect,
                body.to_string().as_bytes(),
            );
            crate::guard::report_answer(&state.bus, id, "", hits,
                &super::redaction(&rt, tail.started.ledger.clone()));
            match changed {
                Some(bytes) => serde_json::from_slice(&bytes)
                    .map_err(|_| failed(web_tools::error("invalid filtered web response")))?,
                None => body,
            }
        } else {
            body
        };
        let bytes = if stream {
            web_tools::wire::stream(req.dialect, &body)
        } else {
            body.to_string().into_bytes()
        };
        let end = ending.as_mut().expect("entered request");
        end.count(bytes.len());
        end.responded(200);
        state.bus.emit(tw_api::Event::RequestHeaders {
            id,
            status: 200,
            ttfb_ms: req.started.elapsed().as_millis() as u64,
        });
        let mut response = (
            [
                (
                    "content-type",
                    if stream {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                ),
                ("x-thinkwatch-web-tools", "core"),
            ],
            bytes,
        )
            .into_response();
        if let Ok(value) =
            axum::http::HeaderValue::from_str(&serde_json::to_string(&trace).unwrap_or_default())
        {
            response
                .headers_mut()
                .insert("x-thinkwatch-web-trace", value);
        }
        Ok(response)
    }
    .await;
    if let Some(end) = ending.take() {
        match &result {
            Ok(response) => end.finished(response.status().as_u16()),
            Err(e) => end.failed(e.source.into(), e.detail.clone()),
        }
    }
    result
}

fn round_request(req: &Inbound, body: &Value) -> Inbound {
    Inbound {
        web_parent: false,
        uri: req.uri.clone(),
        query: req.query.clone(),
        headers: req.headers.clone(),
        body: Bytes::from(body.to_string()),
        version: req.version,
        client_name: req.client_name.clone(),
        api: req.api,
        dialect: req.dialect,
        started: std::time::Instant::now(),
        from: req.from.clone(),
        abort: req.abort.clone(),
        traffic: crate::traffic::Traffic::new(),
        seat: crate::content::Seat::default(),
    }
}

async fn forward_original(
    state: &AppState,
    rt: &Arc<Runtime>,
    req: &Inbound,
    body: &Value,
) -> Result<Response, GatewayError> {
    let mut ending = None;
    let result = super::forward_pipeline(
        state.clone(),
        rt.clone(),
        round_request(req, body),
        state.live.enter(),
        &mut ending,
        Some(body.clone()),
    )
    .await;
    if let Some(end) = ending {
        match &result {
            Ok(response) => end.finished(response.status().as_u16()),
            Err(e) => end.failed(e.source.into(), e.detail.clone()),
        }
    }
    result
}

enum ModelResult {
    Answer(Value),
    Failed(Response),
}

async fn model_loop(
    state: &AppState,
    rt: &Arc<Runtime>,
    req: &Inbound,
    plan: &Plan,
    mut body: Value,
    trace: &mut Vec<tw_api::WebToolTrace>,
) -> Result<ModelResult, GatewayError> {
    plan.rewrite(&mut body).map_err(failed)?;
    let max = plan
        .max_uses
        .unwrap_or(rt.config.web_tools.max_tool_calls)
        .min(rt.config.web_tools.max_tool_calls) as usize;
    let mut used = 0;
    let mut accumulated = Vec::new();
    let mut usage = tw_dialect::ir::Usage::default();
    let mut sources = Vec::new();
    loop {
        let response = forward_original(state, rt, req, &body).await?;
        if !response.status().is_success() {
            // Preserve native status, error details and retry headers.
            return Ok(ModelResult::Failed(response));
        }
        let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .map_err(|_| {
                failed(web_tools::error(
                    "model response was interrupted or exceeded 16 MiB",
                ))
            })?;
        let mut answer: Value = serde_json::from_slice(&bytes).map_err(|_| {
            failed(web_tools::error(
                "model did not return a complete JSON response",
            ))
        })?;
        let next_usage = if req.dialect == Dialect::Anthropic {
            tw_dialect::anthropic::response::usage(&answer["usage"])
        } else {
            tw_dialect::responses::response::usage(&answer["usage"])
        };
        usage.input = usage.input.saturating_add(next_usage.input);
        usage.output = usage.output.saturating_add(next_usage.output);
        usage.cache_read = usage.cache_read.saturating_add(next_usage.cache_read);
        usage.cache_write = usage.cache_write.saturating_add(next_usage.cache_write);
        usage.reasoning = usage.reasoning.saturating_add(next_usage.reasoning);
        usage.cache_1h |= next_usage.cache_1h;
        let calls = plan.calls(&answer).map_err(failed)?;
        let key = if req.dialect == Dialect::Anthropic {
            "content"
        } else {
            "output"
        };
        let items = answer[key]
            .as_array()
            .cloned()
            .ok_or_else(|| failed(web_tools::error("model response has no content")))?;
        let external = items.iter().any(|item| {
            matches!(
                item["type"].as_str(),
                Some("tool_use" | "function_call" | "custom_tool_call")
            ) && ![protocol::SEARCH, protocol::OPEN, protocol::FIND]
                .contains(&item["name"].as_str().unwrap_or(""))
        });
        if used + calls.len() > max {
            return Err(failed(web_tools::error(
                "model exceeded the configured web-tool call limit",
            )));
        }
        used += calls.len();
        let mut results = Vec::new();
        for call in calls {
            let result = tokio::select! {
                _ = req.abort.wait() => return Err(GatewayError::aborted()),
                r = web_tools::execute_scoped(rt, &req.client_name, &call.request, &plan.allowed, &plan.blocked) => r,
            }
            .map_err(failed)?;
            trace.push(trace_of(&call.request, &result));
            sources.extend(result.sources.clone());
            rt.web_tools
                .history
                .remember(&req.client_name, req.dialect, &call, &result);
            results.push((call, result));
        }
        // Keep client-executed calls after completed hosted exchanges, so their
        // results on the next turn immediately follow the matching assistant turn.
        let (client_calls, items): (Vec<_>, Vec<_>) = items.into_iter().partition(|item| {
            matches!(
                item["type"].as_str(),
                Some("tool_use" | "function_call" | "custom_tool_call")
            ) && ![protocol::SEARCH, protocol::OPEN, protocol::FIND]
                .contains(&item["name"].as_str().unwrap_or(""))
        });
        for item in items {
            let id = if req.dialect == Dialect::Anthropic {
                item["id"].as_str()
            } else {
                item["call_id"].as_str()
            };
            if let Some((call, result)) = results.iter().find(|(c, _)| Some(c.id.as_str()) == id) {
                accumulated.extend(protocol::native_blocks(req.dialect, call, result));
            } else {
                accumulated.push(item);
            }
        }
        if results.is_empty() || external {
            accumulated.extend(client_calls);
            answer[key] = json!(accumulated);
            answer["usage"] = if req.dialect == Dialect::Anthropic {
                tw_dialect::anthropic::response::usage_json(&usage)
            } else {
                tw_dialect::responses::response::usage_json(&usage)
            };
            if req.dialect == Dialect::Responses {
                citations(&mut answer, &sources);
            }
            return Ok(ModelResult::Answer(answer));
        }
        plan.append(&mut body, &answer, &results).map_err(failed)?;
        if body.to_string().len() > 16 * 1024 * 1024 {
            return Err(failed(web_tools::error(
                "web-tool conversation exceeded 16 MiB",
            )));
        }
    }
}

fn citations(answer: &mut Value, sources: &[tw_api::WebSource]) {
    for item in answer["output"].as_array_mut().into_iter().flatten() {
        for part in item["content"].as_array_mut().into_iter().flatten() {
            if part["type"] != "output_text" {
                continue;
            }
            let text = part["text"].as_str().unwrap_or("").to_string();
            let mut annotations = part["annotations"].as_array().cloned().unwrap_or_default();
            for source in sources {
                if let Some(start) = text.find(&source.url) {
                    let start = text[..start].chars().count();
                    let end = start + source.url.chars().count();
                    if !annotations.iter().any(|a| a["url"] == source.url) {
                        annotations.push(json!({"type":"url_citation","url":source.url,"title":source.title,"start_index":start,"end_index":end}));
                    }
                }
            }
            part["annotations"] = json!(annotations);
        }
    }
}

#[cfg(test)]
mod tests;

fn trace_of(
    request: &tw_api::WebToolRequest,
    result: &tw_api::WebToolResult,
) -> tw_api::WebToolTrace {
    let mut engines = Vec::new();
    for source in &result.sources {
        if let Some(engine) = source.engine
            && !engines.contains(&engine)
        {
            engines.push(engine);
        }
    }
    tw_api::WebToolTrace {
        action: match request {
            tw_api::WebToolRequest::Search { .. } => "search",
            tw_api::WebToolRequest::OpenPage { .. } => "open_page",
            tw_api::WebToolRequest::FindInPage { .. } => "find_in_page",
        }
        .into(),
        engines,
        cached: result.cached,
        elapsed_ms: result.elapsed_ms,
        sources: result.sources.len() as u32,
        warnings: result.warnings.len() as u32,
    }
}
