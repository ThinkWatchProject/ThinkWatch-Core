//! Try an alternative response mode after an eligible request rejection.
//! The client mode stays on the conversion session; only the outbound request
//! changes. An accepted alternative is remembered locally until reload or expiry.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;
use tw_dialect::ir::Dialect;

// Fixed expiry: successful cached uses do not postpone retrying the default.
const TTL_MS: u64 = 30 * 60 * 1000;
const MAX: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    provider: String,
    identity: String,
    model: String,
    dialect: Dialect,
    path: String,
    options: String,
}

impl Key {
    pub fn of(
        provider: &tw_config::Provider,
        model: &str,
        dialect: Dialect,
        path: &str,
        body: &[u8],
    ) -> Option<Self> {
        let mut options: Value = serde_json::from_slice(body).ok()?;
        if options.get("stream").is_some_and(|v| !v.is_boolean())
            || options
                .get("stream_options")
                .is_some_and(|v| !v.is_object() && !v.is_null())
        {
            return None;
        }
        // The response IR represents one text/reasoning/tool answer. Do not
        // silently discard richer native response contracts on a mode change.
        if options
            .get("n")
            .and_then(Value::as_u64)
            .is_some_and(|n| n > 1)
            || options.get("logprobs").and_then(Value::as_bool) == Some(true)
            || options.get("top_logprobs").is_some_and(|v| !v.is_null())
            || options.get("audio").is_some_and(|v| !v.is_null())
            || options
                .get("modalities")
                .and_then(Value::as_array)
                .is_some_and(|a| a.iter().any(|v| v.as_str() != Some("text")))
            || options
                .pointer("/generationConfig/candidateCount")
                .and_then(Value::as_u64)
                .is_some_and(|n| n > 1)
            || options
                .pointer("/generationConfig/responseLogprobs")
                .and_then(Value::as_bool)
                == Some(true)
            || options
                .pointer("/generationConfig/responseModalities")
                .and_then(Value::as_array)
                .is_some_and(|a| a.iter().any(|v| v.as_str() != Some("TEXT")))
            || options
                .get("include")
                .and_then(Value::as_array)
                .is_some_and(|a| {
                    a.iter()
                        .any(|v| v.as_str().is_some_and(|s| s.contains("logprobs")))
                })
        {
            return None;
        }
        let fields = options.as_object_mut()?;
        // Keep capability-affecting options (tools, thinking, modalities, etc.)
        // isolated without retaining prompts or splitting the two stream modes.
        for name in [
            "model",
            "stream",
            "stream_options",
            "messages",
            "input",
            "instructions",
            "system",
            "contents",
        ] {
            fields.remove(name);
        }
        let mut identity = blake3::Hasher::new();
        let mut add = |value: &str| {
            identity.update(&(value.len() as u64).to_le_bytes());
            identity.update(value.as_bytes());
        };
        add(&provider.base_url);
        add(&provider.credential_identity());
        if let Some(key) = &provider.key {
            add(&key.resolve().ok()?);
        }
        if let Some(oauth) = &provider.oauth {
            add(&oauth.refresh);
        }
        for header in provider.headers.iter() {
            add(&header.name.to_ascii_lowercase());
            add(&header.value.resolve().ok()?);
        }
        if let Some(aws) = &provider.aws {
            for secret in [
                &aws.access_key_id,
                &aws.secret_access_key,
                &aws.session_token,
            ]
            .into_iter()
            .flatten()
            {
                add(&secret.resolve().ok()?);
            }
        }
        Some(Self {
            provider: provider.name.clone(),
            identity: identity.finalize().to_hex().to_string(),
            model: model.into(),
            dialect,
            path: match dialect {
                Dialect::Gemini => path.replace(":streamGenerateContent", ":generateContent"),
                Dialect::Bedrock => path.trim_end_matches("-stream").into(),
                _ => path.into(),
            },
            options: blake3::hash(options.to_string().as_bytes())
                .to_hex()
                .to_string(),
        })
    }
}

#[derive(Default)]
pub struct Cache(Mutex<HashMap<Key, (bool, u64)>>);

impl Cache {
    pub fn lookup(&self, key: &Key, now: u64) -> Option<bool> {
        let mut entries = self.0.lock().unwrap();
        entries.retain(|_, (_, at)| now.saturating_sub(*at) < TTL_MS);
        entries.get(key).map(|(stream, _)| *stream)
    }

    pub fn accepted(&self, key: Key, stream: bool, now: u64) {
        let mut entries = self.0.lock().unwrap();
        if entries.len() >= MAX
            && !entries.contains_key(&key)
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(k, _)| k.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(key, (stream, now));
    }

    pub fn forget(&self, key: &Key) {
        self.0.lock().unwrap().remove(key);
    }
}

pub fn requested(dialect: Dialect, path: &str, body: &[u8]) -> Option<bool> {
    match dialect {
        Dialect::Gemini => Some(path.ends_with(":streamGenerateContent")),
        Dialect::Bedrock => Some(path.ends_with("/converse-stream")),
        _ => {
            let v: Value = serde_json::from_slice(body).ok()?;
            match v.get("stream") {
                None => Some(false),
                Some(v) => v.as_bool(),
            }
        }
    }
}

/// Change the wire mode without round-tripping any prompt through the IR.
/// Gemini and Bedrock put the mode in the endpoint, not in the JSON body.
pub fn rewrite(
    dialect: Dialect,
    path: &mut String,
    query: &mut Option<String>,
    body: &[u8],
    stream: bool,
) -> Option<Vec<u8>> {
    match dialect {
        Dialect::Gemini => {
            let base = path
                .strip_suffix(":streamGenerateContent")
                .or_else(|| path.strip_suffix(":generateContent"))?;
            *path = format!(
                "{base}:{}",
                if stream {
                    "streamGenerateContent"
                } else {
                    "generateContent"
                }
            );
            let mut args: Vec<_> = query
                .as_deref()
                .unwrap_or_default()
                .split('&')
                .filter(|s| !s.is_empty() && s.split('=').next() != Some("alt"))
                .map(str::to_owned)
                .collect();
            if stream {
                args.push("alt=sse".into());
            }
            *query = (!args.is_empty()).then(|| args.join("&"));
            Some(body.to_vec())
        }
        Dialect::Bedrock => {
            let base = path
                .strip_suffix("/converse-stream")
                .or_else(|| path.strip_suffix("/converse"))?;
            *path = format!(
                "{base}/{}",
                if stream {
                    "converse-stream"
                } else {
                    "converse"
                }
            );
            Some(body.to_vec())
        }
        _ => {
            let mut v: Value = serde_json::from_slice(body).ok()?;
            let fields = v.as_object_mut()?;
            fields.insert("stream".into(), Value::Bool(stream));
            if !stream {
                fields.remove("stream_options");
            } else if dialect == Dialect::Chat {
                // A whole-answer client expects usage too. Chat only sends the
                // final usage chunk when explicitly requested.
                let options = fields
                    .entry("stream_options")
                    .or_insert_with(|| serde_json::json!({}));
                if options.is_null() {
                    *options = serde_json::json!({});
                }
                options
                    .as_object_mut()?
                    .insert("include_usage".into(), Value::Bool(true));
            }
            serde_json::to_vec(&v).ok()
        }
    }
}

/// A bounded experiment, not a claim that the requested mode is unsupported.
/// Known repairs run first. Reject non-mode parameter errors and recognizable
/// account, capacity or timeout errors, even when an upstream mislabels them.
pub fn can_probe(
    status: u16,
    headers: &http::HeaderMap,
    body: &[u8],
    verdict: crate::failure::Verdict,
) -> bool {
    use crate::failure::{Cause, Verdict};
    if !matches!(status, 400 | 422 | 501)
        || body.len() > crate::failure::BODY_PEEK
        || crate::cache_marks::refusal(body)
        || !matches!(verdict, Verdict::ClientError)
            && !(status == 501 && verdict == Verdict::Failed(Cause::Unexplained))
        || headers.contains_key("retry-after")
        || headers.contains_key("retry-after-ms")
        || headers.get("x-should-retry").is_some_and(|v| v == "true")
    {
        return false;
    }
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let error = match v.get("error") {
        Some(e) if e.is_object() || e.is_string() => e,
        None if ["message", "detail", "code", "type", "status"]
            .iter()
            .any(|name| v.get(name).is_some_and(Value::is_string)) =>
        {
            &v
        }
        _ => return false,
    };
    if let Some(param) = error.get("param").and_then(Value::as_str)
        && !param.is_empty()
        && param != "stream"
    {
        return false;
    }
    if error
        .get("message")
        .and_then(Value::as_str)
        .is_some_and(names_other_parameter)
    {
        return false;
    }
    // Read error fields only: an echoed prompt or metadata must not affect
    // eligibility. No list of positive English mode-refusal phrases is needed.
    let text = if let Some(text) = error.as_str() {
        text.to_lowercase()
    } else {
        ["code", "type", "status", "message", "detail"]
            .into_iter()
            .filter_map(|name| error.get(name).and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\0")
            .to_lowercase()
    };
    ![
        "rate limit",
        "rate_limit",
        "ratelimit",
        "too_many_requests",
        "quota",
        "resource_exhausted",
        "overload",
        "authentication",
        "unauthenticated",
        "unauthorized",
        "permission",
        "forbidden",
        "api key",
        "api_key",
        "credential",
        "access denied",
        "access_denied",
        "billing",
        "payment",
        "balance",
        "credit",
        "timeout",
        "timed out",
        "deadline_exceeded",
        "content_policy",
        "content_filter",
        "safety",
        "限流",
        "频率",
        "额度",
        "余额",
        "欠费",
        "鉴权",
        "认证",
        "凭据",
        "权限",
        "超时",
    ]
    .iter()
    .any(|word| text.contains(word))
}

/// Some APIs put a field path in the message instead of `param`. Keep known
/// request validation errors intact, including explicit reasoning budgets.
fn names_other_parameter(message: &str) -> bool {
    let text = message.trim().to_lowercase();
    if text.contains("stream") {
        return false;
    }
    let field = text
        .trim_start_matches(['`', '\'', '"'])
        .split(|c: char| c.is_whitespace() || matches!(c, ':' | '`' | '\'' | '"'))
        .next()
        .unwrap_or_default();
    let root = field.split(['.', '[', '/']).next().unwrap_or_default();
    matches!(
        root,
        "messages"
            | "input"
            | "contents"
            | "system"
            | "instructions"
            | "tools"
            | "max_tokens"
            | "max_output_tokens"
            | "max_completion_tokens"
            | "temperature"
            | "top_p"
            | "top_k"
            | "stop"
            | "stop_sequences"
            | "thinking"
            | "reasoning"
            | "reasoning_effort"
            | "output_config"
            | "generationconfig"
            | "inferenceconfig"
    )
}

/// Evidence of a complete stream, used only while learning a new mode.
pub struct Completion {
    dialect: Dialect,
    decoder: tw_dialect::frame::Decoder,
    ended: bool,
}

impl Completion {
    pub fn new(dialect: Dialect) -> Self {
        Self {
            dialect,
            decoder: Default::default(),
            ended: false,
        }
    }

    fn frames(&mut self, frames: Vec<tw_dialect::frame::Frame>) {
        for f in frames {
            self.ended |= tw_dialect::convert::ends_answer(self.dialect, &f);
            if self.dialect == Dialect::Gemini {
                self.ended |= serde_json::from_str::<Value>(&f.data)
                    .ok()
                    .is_some_and(|v| {
                        v.get("candidates")
                            .and_then(Value::as_array)
                            .is_some_and(|a| {
                                a.iter().any(|c| {
                                    c.get("finishReason").and_then(Value::as_str).is_some()
                                })
                            })
                    });
            } else if self.dialect == Dialect::Bedrock {
                self.ended |= f.event.as_deref() == Some("messageStop");
            }
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        let frames = self.decoder.feed(bytes);
        self.frames(frames);
    }

    pub fn finish(mut self) -> bool {
        let frames = self.decoder.flush();
        self.frames(frames);
        self.ended
    }
}

pub fn whole_answer(dialect: Dialect, body: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    if v.get("error").is_some_and(|e| !e.is_null())
        || v.get("status")
            .and_then(Value::as_str)
            .is_some_and(|s| matches!(s, "failed" | "cancelled"))
    {
        return false;
    }
    let field = match dialect {
        Dialect::Chat => "choices",
        Dialect::Anthropic => "content",
        Dialect::Responses => "output",
        Dialect::Gemini => "candidates",
        Dialect::Bedrock => {
            return v
                .pointer("/output/message/content")
                .is_some_and(Value::is_array);
        }
    };
    v.get(field).is_some_and(Value::is_array)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn probes_unknown_rejections_without_requiring_mode_error_phrases() {
        let headers = http::HeaderMap::new();
        for body in [
            json!({"error":{"message":"Use the other response framing."}}),
            json!({"error":{"message":"请换一种返回方式。"}}),
            json!({"error":{"code":"vendor_1027"}}),
            json!({"message":"Request rejected", "type":"invalid_request_error"}),
            json!({"error":"Request rejected"}),
            json!({"error":{"message":"Tools require streaming for this model"}}),
        ] {
            for status in [400, 422, 501] {
                let body = body.to_string();
                let verdict = crate::failure::classify(status, &headers, body.as_bytes(), 0);
                assert!(
                    can_probe(status, &headers, body.as_bytes(), verdict),
                    "{body}"
                );
            }
        }
        for status in [200, 401, 402, 403, 404, 408, 429, 500, 502, 503] {
            assert!(!can_probe(
                status,
                &headers,
                br#"{"error":{}}"#,
                crate::failure::classify(status, &headers, b"{}", 0)
            ));
        }
        for body in [
            json!({"error":{"message":"Rate limit exceeded"}}),
            json!({"error":{"code":"rate_limit_exceeded", "message":"opaque"}}),
            json!({"error":{"type":"authentication_error", "message":"opaque"}}),
            json!({"error":{"status":"RESOURCE_EXHAUSTED", "message":"opaque"}}),
            json!({"error":{"message":"Quota exceeded; only streaming allowed"}}),
            json!({"error":{"message":"invalid api key"}}),
            json!({"error":{"message":"Insufficient balance"}}),
            json!({"error":{"message":"Model not found"}}),
            json!({"error":{"message":"Request timed out"}}),
            json!({"error":{"message":"请求频率过高"}}),
            json!({"error":{"message":"messages.0.content: field required"}}),
            json!({"error":{"message":"max_output_tokens is too large"}}),
            json!({"error":{"message":"thinking.budget_tokens: budget too large"}}),
            json!({"message":"cachePoint is not supported for this model"}),
            json!({"error":{"param":"messages", "message":"invalid messages"}}),
            json!({"error":{"param":"stream_options", "message":"not supported"}}),
            json!({"choices":[{"message":{"content":"only streaming"}}]}),
            json!({"error":null}),
        ] {
            let body = body.to_string();
            let verdict = crate::failure::classify(400, &headers, body.as_bytes(), 0);
            assert!(
                !can_probe(400, &headers, body.as_bytes(), verdict),
                "{body}"
            );
        }
        for name in ["retry-after", "retry-after-ms", "x-should-retry"] {
            let mut headers = http::HeaderMap::new();
            headers.insert(http::HeaderName::from_static(name), "true".parse().unwrap());
            assert!(!can_probe(
                400,
                &headers,
                br#"{"error":{}}"#,
                crate::failure::Verdict::ClientError
            ));
        }
        for body in [
            b"not JSON".as_slice(),
            &[b'x'; crate::failure::BODY_PEEK + 1],
        ] {
            assert!(!can_probe(
                400,
                &headers,
                body,
                crate::failure::Verdict::ClientError
            ));
        }
    }

    #[test]
    fn cache_expires_and_isolates_models_accounts_apis_and_options() {
        let p = tw_config::Provider {
            name: "up".into(),
            base_url: "https://relay.example".into(),
            ..Default::default()
        };
        let key = Key::of(&p, "m", Dialect::Chat, "/v1/chat/completions", b"{}").unwrap();
        let cache = Cache::default();
        cache.accepted(key.clone(), true, 1);
        assert_eq!(cache.lookup(&key, 2), Some(true));
        for (model, dialect, body) in [
            ("other", Dialect::Chat, "{}"),
            ("m", Dialect::Responses, "{}"),
            ("m", Dialect::Chat, r#"{"tools":[{}]}"#),
        ] {
            assert_eq!(
                cache.lookup(
                    &Key::of(&p, model, dialect, "/v1/chat/completions", body.as_bytes()).unwrap(),
                    2
                ),
                None
            );
        }
        let mut changed = p.clone();
        changed.key = Some("other-account".into());
        assert_ne!(
            key,
            Key::of(&changed, "m", Dialect::Chat, "/v1/chat/completions", b"{}").unwrap()
        );
        assert_eq!(cache.lookup(&key, TTL_MS), Some(true));
        assert_eq!(cache.lookup(&key, TTL_MS + 1), None);
        for i in 0..MAX + 1 {
            cache.accepted(
                Key::of(
                    &p,
                    &format!("m{i}"),
                    Dialect::Chat,
                    "/v1/chat/completions",
                    b"{}",
                )
                .unwrap(),
                true,
                i as u64,
            );
        }
        assert_eq!(cache.0.lock().unwrap().len(), MAX);
    }
}
