use crate::{AiProvider, ProviderBase};
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tw_protocol::SseStreamExt;
use tw_types::*;

pub struct GoogleProvider {
    pub base: ProviderBase,
}

impl GoogleProvider {
    pub fn new(base_url: String) -> Self {
        Self {
            base: ProviderBase::new(base_url),
        }
    }

    pub fn with_custom_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.base = self.base.with_custom_headers(headers);
        self
    }
}

// ---------- Gemini API types ----------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeminiContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiResponse {
    candidates: Option<Vec<GeminiCandidate>>,
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiCandidate {
    content: Option<GeminiContent>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiUsageMetadata {
    prompt_token_count: Option<u32>,
    candidates_token_count: Option<u32>,
    total_token_count: Option<u32>,
}

// ---------- Conversion ----------

fn convert_request(req: &ChatCompletionRequest) -> GeminiRequest {
    let mut system_instruction: Option<GeminiContent> = None;
    let mut contents = Vec::new();

    for msg in &req.messages {
        let text = match &msg.content {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };

        if msg.role == "system" {
            system_instruction = Some(GeminiContent {
                role: None,
                parts: vec![GeminiPart { text: Some(text) }],
            });
        } else {
            let role = match msg.role.as_str() {
                "assistant" => "model",
                _ => "user",
            };
            contents.push(GeminiContent {
                role: Some(role.to_string()),
                parts: vec![GeminiPart { text: Some(text) }],
            });
        }
    }

    GeminiRequest {
        contents,
        system_instruction,
        generation_config: Some(GeminiGenerationConfig {
            temperature: req.temperature,
            max_output_tokens: req.max_tokens,
        }),
    }
}

/// Gemini 的停止原因 → OpenAI 的写法。
///
/// 整包和流式都要用：**流式那条路原先什么都不映射**，见 `stream_chat_completion`。
fn finish_reason_of(raw: &str) -> String {
    match raw {
        "STOP" => "stop".to_string(),
        "MAX_TOKENS" => "length".to_string(),
        other => other.to_lowercase(),
    }
}

/// Gemini 报的用量 → 我们的 `Usage`。
///
/// **流式的每一帧都带着累计值**，所以每一帧都要填：调用方取的是最后
/// 一个带用量的块，而那一帧上的数就是这次请求的最终用量。
fn usage_of(u: GeminiUsageMetadata) -> Usage {
    Usage {
        prompt_tokens: u.prompt_token_count.unwrap_or(0),
        completion_tokens: u.candidates_token_count.unwrap_or(0),
        total_tokens: u.total_token_count.unwrap_or(0),
    }
}

fn convert_response(resp: GeminiResponse, model: &str) -> ChatCompletionResponse {
    let (text, finish_reason) = resp
        .candidates
        .and_then(|c| c.into_iter().next())
        .map(|c| {
            let text = c
                .content
                .and_then(|content| {
                    content
                        .parts
                        .into_iter()
                        .filter_map(|p| p.text)
                        .collect::<Vec<_>>()
                        .first()
                        .cloned()
                })
                .unwrap_or_default();
            let reason = c.finish_reason.as_deref().map(finish_reason_of);
            (text, reason)
        })
        .unwrap_or_default();

    let usage = resp.usage_metadata.map(usage_of);

    ChatCompletionResponse {
        id: format!("gemini-{}", uuid::Uuid::new_v4()),
        object: "chat.completion".to_string(),
        created: chrono::Utc::now().timestamp(),
        model: model.to_string(),
        choices: vec![Choice {
            index: 0,
            message: ChatMessage {
                role: "assistant".to_string(),
                content: serde_json::Value::String(text),
                ..Default::default()
            },
            finish_reason,
        }],
        usage,
    }
}

/// 流式的一帧 → 一个块。
///
/// **用量和停止原因原先在这里被丢掉**：两个字段都写死 `None`，而 Gemini
/// 每一帧都带着累计用量。丢掉的后果不是少一个数字 —— 调用方拿不到用量
/// 就只能按字符数估算计费，于是经 Gemini 的流式请求从来没有按真实用量
/// 记过账。
fn convert_chunk(resp: GeminiResponse, chunk_id: &str, model: &str) -> ChatCompletionChunk {
    // 每一帧都填：用量是累计的，调用方取最后一个带用量的块
    let usage = resp.usage_metadata.map(usage_of);
    let (text, finish_reason) = resp
        .candidates
        .and_then(|c| c.into_iter().next())
        .map(|c| {
            let text = c
                .content
                .and_then(|content| content.parts.into_iter().next())
                .and_then(|p| p.text)
                .unwrap_or_default();
            (text, c.finish_reason.as_deref().map(finish_reason_of))
        })
        .unwrap_or_default();

    ChatCompletionChunk {
        id: chunk_id.to_string(),
        object: "chat.completion.chunk".to_string(),
        created: chrono::Utc::now().timestamp(),
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: serde_json::json!({ "content": text }),
            finish_reason,
        }],
        usage,
    }
}

// ---------- AiProvider ----------

impl AiProvider for GoogleProvider {
    fn name(&self) -> &str {
        "google"
    }

    async fn chat_completion(
        &self,
        request: ChatCompletionRequest,
        ctx: CallCtx,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let model = request.model.clone();
        let gemini_req = convert_request(&request);

        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.base.base_url, model
        );

        let builder = self
            .base
            .apply_custom_headers(self.base.client.post(&url), &ctx)
            .json(&gemini_req);

        let resp = ProviderBase::send(builder).await?;
        let resp = ProviderBase::check_status(resp, "Gemini").await?;

        let gemini_resp: GeminiResponse = resp
            .json()
            .await
            .map_err(|e| GatewayError::ProviderError(e.to_string()))?;

        Ok(convert_response(gemini_resp, &model))
    }

    fn stream_chat_completion(
        &self,
        request: ChatCompletionRequest,
        ctx: CallCtx,
    ) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>> {
        let client = self.base.client.clone();
        let base_url = self.base.base_url.clone();
        let model = request.model.clone();
        let headers = self.base.resolve_headers(&ctx);

        let gemini_req = convert_request(&request);

        Box::pin(async_stream::stream! {
            let url = format!(
                "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
                base_url, model
            );

            let builder = ProviderBase::apply_headers(client.post(&url), &headers).json(&gemini_req);

            let resp = match ProviderBase::send(builder).await {
                Ok(r) => r,
                Err(e) => { yield Err(e); return; }
            };
            let resp = match ProviderBase::check_status(resp, "Gemini").await {
                Ok(r) => r,
                Err(e) => { yield Err(e); return; }
            };

                        use futures::StreamExt;
            let mut stream = resp.bytes_stream().sse_events();
            let chunk_id = format!("gemini-{}", uuid::Uuid::new_v4());

            while let Some(event) = stream.next().await {
                let event = match event {
                    Ok(e) => e,
                    Err(e) => {
                        yield Err(GatewayError::NetworkError(e.to_string()));
                        return;
                    }
                };

                let data = event.data.trim().to_string();
                if data.is_empty() || data == "[DONE]" {
                    break;
                }

                if let Ok(gemini_resp) = serde_json::from_str::<GeminiResponse>(&data) {
                    yield Ok(convert_chunk(gemini_resp, &chunk_id, &model));
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(json: &str) -> GeminiResponse {
        serde_json::from_str(json).expect("frame parses")
    }

    #[test]
    fn a_streamed_frame_carries_the_usage_gemini_reported() {
        // 这正是原先丢掉的东西：帧里有用量，块上却是 None，于是计费
        // 退回按字符数估算
        let chunk = convert_chunk(
            frame(
                r#"{"candidates":[{"content":{"parts":[{"text":"hi"}]}}],
                    "usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":3,"totalTokenCount":14}}"#,
            ),
            "id",
            "gemini-2.5-pro",
        );

        let usage = chunk.usage.expect("the frame reported usage");
        assert_eq!(usage.prompt_tokens, 11);
        assert_eq!(usage.completion_tokens, 3);
        assert_eq!(usage.total_tokens, 14);
    }

    #[test]
    fn a_frame_without_usage_reports_none_rather_than_zeros() {
        // 早期的帧不带用量。**零和「没说」不是一回事** —— 报成零会让
        // 调用方以为这次请求真的没花 token
        let chunk = convert_chunk(
            frame(r#"{"candidates":[{"content":{"parts":[{"text":"hi"}]}}]}"#),
            "id",
            "gemini-2.5-pro",
        );
        assert!(chunk.usage.is_none());
    }

    #[test]
    fn the_last_frames_cumulative_count_is_the_one_that_counts() {
        // Gemini 每一帧都报累计值，所以每一帧都要填：调用方取最后一个
        // 带用量的块，而那一帧上的数就是这次请求的最终用量
        let first = convert_chunk(
            frame(
                r#"{"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":1,"totalTokenCount":12}}"#,
            ),
            "id",
            "m",
        );
        let last = convert_chunk(
            frame(
                r#"{"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":9,"totalTokenCount":20}}"#,
            ),
            "id",
            "m",
        );
        assert_eq!(first.usage.expect("first").completion_tokens, 1);
        assert_eq!(last.usage.expect("last").completion_tokens, 9);
    }

    #[test]
    fn a_streamed_frame_translates_the_stop_reason() {
        // 和用量一样，这个字段原先也写死 None
        let stop = convert_chunk(
            frame(r#"{"candidates":[{"finishReason":"STOP"}]}"#),
            "id",
            "m",
        );
        assert_eq!(stop.choices[0].finish_reason.as_deref(), Some("stop"));

        let cut = convert_chunk(
            frame(r#"{"candidates":[{"finishReason":"MAX_TOKENS"}]}"#),
            "id",
            "m",
        );
        assert_eq!(cut.choices[0].finish_reason.as_deref(), Some("length"));

        let unknown = convert_chunk(
            frame(r#"{"candidates":[{"finishReason":"SAFETY"}]}"#),
            "id",
            "m",
        );
        assert_eq!(unknown.choices[0].finish_reason.as_deref(), Some("safety"));
    }

    #[test]
    fn the_whole_response_and_a_frame_agree_on_the_numbers() {
        // 两条路读的是同一个字段。它们曾经不一致过 —— 整包读了，
        // 流式没读
        let json = r#"{"candidates":[{"content":{"parts":[{"text":"hi"}]},"finishReason":"STOP"}],
                       "usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":2,"totalTokenCount":9}}"#;
        let whole = convert_response(frame(json), "m");
        let streamed = convert_chunk(frame(json), "id", "m");

        let w = whole.usage.expect("whole");
        let s = streamed.usage.expect("streamed");
        assert_eq!(
            (w.prompt_tokens, w.completion_tokens),
            (s.prompt_tokens, s.completion_tokens)
        );
        assert_eq!(
            whole.choices[0].finish_reason,
            streamed.choices[0].finish_reason
        );
    }
}
