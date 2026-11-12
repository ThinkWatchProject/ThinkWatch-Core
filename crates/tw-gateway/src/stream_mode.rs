//! Negotiate an upstream's response mode only after an explicit rejection.
//! The client mode stays on the conversion session; only the outbound request
//! changes. An accepted alternative is remembered locally until reload or expiry.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;
use tw_dialect::ir::Dialect;

const TTL_MS: u64 = 24 * 60 * 60 * 1000;
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

/// Only a top-level API error describing the actual rejected mode is evidence.
/// Never infer capabilities from rate limits, answer text, or stream_options.
pub fn refusal(status: u16, body: &[u8], sent_stream: bool) -> bool {
    if !matches!(status, 400 | 422 | 501) || body.len() > 64 * 1024 {
        return false;
    }
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let e = v.get("error").filter(|e| e.is_object()).unwrap_or(&v);
    if let Some(param) = e.get("param").and_then(Value::as_str)
        && param != "stream"
    {
        return false;
    }
    let message = e.get("message").and_then(Value::as_str).unwrap_or_default();
    let text = message.to_ascii_lowercase();
    if text.contains("rate limit") || text.contains("quota") || text.contains("overload") {
        return false;
    }
    let code = e.get("code").and_then(Value::as_str).unwrap_or_default();
    let codes: &[&str] = if sent_stream {
        &[
            "streaming_not_supported",
            "stream_not_supported",
            "unsupported_streaming",
        ]
    } else {
        &[
            "non_streaming_not_supported",
            "stream_required",
            "streaming_required",
        ]
    };
    if codes.contains(&code) {
        return true;
    }
    // Intentionally narrow phrases: "streaming" inside "non-streaming" must
    // never be mistaken for a refusal in the opposite direction.
    let text = text
        .replace("non streaming", "non-streaming")
        .replace("nonstreaming", "non-streaming");
    let phrases: &[&str] = if sent_stream {
        &[
            "streaming is not supported",
            "streaming not supported",
            "streaming is unsupported",
            "does not support streaming",
            "doesn't support streaming",
            "only supports non-streaming",
            "only non-streaming requests are supported",
            "only non-streaming responses are supported",
            "only non-streaming is supported",
            "supports only non-streaming",
            "stream must be false",
            "stream must be set to false",
            "stream=false is required",
        ]
    } else {
        &[
            "non-streaming is not supported",
            "non-streaming not supported",
            "non-streaming is unsupported",
            "does not support non-streaming",
            "doesn't support non-streaming",
            "only supports streaming",
            "only streaming requests are supported",
            "only streaming responses are supported",
            "only streaming is supported",
            "supports only streaming",
            "stream must be true",
            "stream must be set to true",
            "stream=true is required",
        ]
    };
    phrases.iter().any(|p| {
        text.match_indices(p).any(|(at, _)| {
            at == 0
                || !text.as_bytes()[at - 1].is_ascii_alphanumeric()
                    && !matches!(text.as_bytes()[at - 1], b'-' | b'_')
        })
    })
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
    fn only_explicit_refusals_for_the_sent_mode_trigger_a_switch() {
        let error = |text| json!({"error":{"message":text}}).to_string();
        for (stream, text) in [
            (false, "This model only supports streaming."),
            (false, "Non-streaming is not supported."),
            (false, "stream must be set to true"),
            (true, "This model does not support streaming."),
            (true, "stream must be false"),
            (true, "Only non-streaming requests are supported."),
        ] {
            assert!(refusal(400, error(text).as_bytes(), stream), "{text}");
            assert!(!refusal(400, error(text).as_bytes(), !stream), "{text}");
            for status in [200, 401, 403, 404, 429, 500, 503] {
                assert!(!refusal(status, error(text).as_bytes(), stream));
            }
        }
        for text in [
            "Rate limit exceeded",
            "streaming failed",
            "invalid stream_options",
            "Quota exceeded; only streaming allowed",
        ] {
            for stream in [false, true] {
                assert!(!refusal(400, error(text).as_bytes(), stream));
            }
        }
        assert!(!refusal(
            400,
            br#"{"choices":[{"message":{"content":"only streaming"}}]}"#,
            false
        ));
        assert!(!refusal(
            400,
            br#"{"error":{"param":"stream_options","message":"streaming not supported"}}"#,
            true
        ));
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
