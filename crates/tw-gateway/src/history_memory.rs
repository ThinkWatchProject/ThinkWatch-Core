//! Evidence-based Chat history adaptation. A rejected field is not evidence for
//! another encoding. Keep the original request and omit only the proven extra
//! field on a separately encoded attempt; remember it only after acceptance.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;
use tw_dialect::ir::{Part, Request, Vendor};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryField {
    ReasoningContent,
    Reasoning,
}

const TTL_MS: u64 = 24 * 60 * 60 * 1000;
const MAX: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    provider: String,
    model: String,
    path: String,
    identity: String,
    mode: String,
    tools: bool,
    thinking: bool,
}

impl Key {
    pub fn of(p: &tw_config::Provider, model: &str, path: &str, body: &[u8]) -> Option<Self> {
        let v: Value = serde_json::from_slice(body).ok()?;
        let mode = serde_json::json!({
            "tools": v.get("tools"), "tool_choice": v.get("tool_choice"),
            "thinking": v.get("thinking"), "reasoning": v.get("reasoning"),
            "reasoning_effort": v.get("reasoning_effort"),
        });
        // Runtime-local entries already expire on reload. Also isolate resolved
        // credentials/headers in case an environment-backed value changes.
        let mut identity = p.clone();
        identity.reasoning_history_group = None;
        Some(Self {
            provider: p.name.clone(),
            model: model.into(),
            path: path.into(),
            identity: crate::translate::history_target(&identity, model).origin?,
            mode: blake3::hash(mode.to_string().as_bytes())
                .to_hex()
                .to_string(),
            tools: v
                .get("tools")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty())
                || v.get("messages")
                    .and_then(Value::as_array)
                    .is_some_and(|messages| {
                        messages.iter().any(|m| {
                            m.get("tool_calls").is_some()
                                || m.get("role").and_then(Value::as_str) == Some("tool")
                        })
                    }),
            thinking: v.get("thinking").is_some()
                || v.get("reasoning").is_some()
                || v.get("reasoning_effort").is_some(),
        })
    }
}

#[derive(Clone)]
struct Entry {
    fields: Vec<HistoryField>,
    at_ms: u64,
}
#[derive(Default)]
struct Inner {
    entries: HashMap<Key, Entry>,
}
#[derive(Default)]
pub struct Cache {
    inner: Mutex<Inner>,
}

#[derive(Clone)]
pub struct Policy {
    pub fields: Vec<HistoryField>,
}

impl Cache {
    pub fn policy(&self, key: &Key, now: u64) -> Policy {
        let mut inner = self.inner.lock().unwrap();
        inner
            .entries
            .retain(|_, e| now.saturating_sub(e.at_ms) < TTL_MS);
        Policy {
            fields: inner
                .entries
                .get(key)
                .map(|e| e.fields.clone())
                .unwrap_or_default(),
        }
    }

    pub fn accepted(&self, key: Key, policy: Policy, now: u64) {
        let mut inner = self.inner.lock().unwrap();
        if policy.fields.is_empty() {
            return;
        }
        if inner.entries.len() >= MAX
            && !inner.entries.contains_key(&key)
            && let Some(oldest) = inner
                .entries
                .iter()
                .min_by_key(|(_, e)| e.at_ms)
                .map(|(k, _)| k.clone())
        {
            inner.entries.remove(&oldest);
        }
        inner.entries.insert(
            key,
            Entry {
                fields: policy.fields,
                at_ms: now,
            },
        );
    }

    /// Internal diagnostics/tests; no control-plane endpoint or user setting.
    pub fn entries_for(&self, provider: &str, now: u64) -> usize {
        let mut inner = self.inner.lock().unwrap();
        inner
            .entries
            .retain(|_, e| now.saturating_sub(e.at_ms) < TTL_MS);
        inner
            .entries
            .keys()
            .filter(|k| k.provider == provider)
            .count()
    }
}

pub fn automatic(p: &tw_config::Provider, model: &str) -> bool {
    use tw_config::ReasoningHistory as H;
    p.model_specs
        .get(model)
        .and_then(|s| s.reasoning_history)
        .filter(|f| *f != H::Auto)
        .unwrap_or(p.reasoning_history)
        == H::Auto
}

pub fn unsigned(request: &Request) -> bool {
    request.messages.iter().flat_map(|m| &m.parts).all(|part| {
        let Part::Thinking(t) = part else {
            return true;
        };
        t.signature.as_ref().is_none_or(|s| {
            if s.vendor == Vendor::Chat {
                tw_dialect::history::ChatHistory::read(s).is_some_and(|h| !h.opaque())
            } else {
                s.value.is_empty()
            }
        })
    })
}

fn field(name: &str) -> Option<HistoryField> {
    match name {
        "reasoning_content" => Some(HistoryField::ReasoningContent),
        "reasoning" => Some(HistoryField::Reasoning),
        _ => None,
    }
}

fn location(v: &Value) -> Option<(usize, HistoryField)> {
    let loc = v.as_array()?;
    let loc = if loc.first()?.as_str() == Some("body") {
        &loc[1..]
    } else {
        loc.as_slice()
    };
    if loc.first()?.as_str() != Some("messages") || !loc.get(1)?.is_u64() {
        return None;
    }
    let index = usize::try_from(loc[1].as_u64()?).ok()?;
    let field = match &loc[2..] {
        [name] => field(name.as_str()?),
        [role, name] if role.as_str() == Some("assistant") => field(name.as_str()?),
        _ => None,
    }?;
    Some((index, field))
}

/// Only known machine-readable request validation schemas, never prose, SSE
/// errors, generic 422s, nested tool arguments, or a model's answer text.
pub fn refusal(status: u16, body: &[u8], sent: &[u8]) -> Option<Vec<HistoryField>> {
    if !matches!(status, 400 | 422) || body.len() > 64 * 1024 {
        return None;
    }
    let v: Value = serde_json::from_slice(body).ok()?;
    let errors = v
        .get("detail")
        .or_else(|| v.get("error").and_then(|e| e.get("detail")))
        .and_then(Value::as_array)?;
    if errors.is_empty() || errors.len() > 32 {
        return None;
    }
    let request: Value = serde_json::from_slice(sent).ok()?;
    let messages = request.get("messages")?.as_array()?;
    let mut fields = Vec::new();
    for e in errors {
        if e.get("type").and_then(Value::as_str) != Some("extra_forbidden") {
            return None;
        }
        let (index, f) = location(e.get("loc")?)?;
        let message = messages.get(index)?;
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return None;
        }
        let name = match f {
            HistoryField::ReasoningContent => "reasoning_content",
            HistoryField::Reasoning => "reasoning",
        };
        message.get(name)?;
        if !fields.contains(&f) {
            fields.push(f);
        }
    }
    Some(fields)
}

/// Remove only explicitly rejected, unsigned assistant fields. Other history,
/// tool call IDs, results and extension fields retain their original values.
pub fn omit(body: &[u8], fields: &[HistoryField]) -> Option<Vec<u8>> {
    if fields.is_empty() {
        return None;
    }
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let messages = v.get_mut("messages")?.as_array_mut()?;
    // Native signed blocks may not have been represented by the decoder.
    if messages.iter().any(|m| {
        m.get("content").and_then(Value::as_array).is_some_and(|a| {
            a.iter().any(|b| {
                b.get("signature")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
                    || b.get("type").and_then(Value::as_str) == Some("redacted_thinking")
            })
        })
    }) {
        return None;
    }
    if messages
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .any(|m| {
            fields.iter().any(|f| {
                let key = match f {
                    HistoryField::ReasoningContent => "reasoning_content",
                    HistoryField::Reasoning => "reasoning",
                };
                m.get(key).is_some_and(|v| !v.is_string() && !v.is_null())
            })
        })
    {
        return None;
    }
    let mut changed = false;
    messages.retain_mut(|m| {
        if m.get("role").and_then(Value::as_str) != Some("assistant") {
            return true;
        }
        let Some(object) = m.as_object_mut() else {
            return true;
        };
        let mut removed = false;
        for f in fields {
            let key = match f {
                HistoryField::ReasoningContent => "reasoning_content",
                HistoryField::Reasoning => "reasoning",
            };
            removed |= object.remove(key).is_some();
        }
        changed |= removed;
        !removed
            || object.get("content").is_some_and(|c| match c {
                Value::Null => false,
                Value::String(s) => !s.is_empty(),
                Value::Array(a) => !a.is_empty(),
                _ => true,
            })
            || object
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty())
            || object
                .keys()
                .any(|k| !matches!(k.as_str(), "role" | "content" | "tool_calls"))
    });
    changed.then(|| serde_json::to_vec(&v).expect("JSON history"))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_precise_request_validation_is_evidence() {
        let wire = json!({"messages": (0..4).map(|_| json!({"role":"assistant","reasoning_content":"thought","reasoning":"other"})).collect::<Vec<_>>()}).to_string();
        let refusal = |status, body: &[u8]| super::refusal(status, body, wire.as_bytes());
        let error = |loc: Value, kind: &str| {
            json!({"detail":[{"type":kind,"loc":loc,"msg":"arbitrary prose"}]}).to_string()
        };
        let exact = error(
            json!(["body", "messages", 3, "assistant", "reasoning_content"]),
            "extra_forbidden",
        );
        assert_eq!(
            refusal(422, exact.as_bytes()),
            Some(vec![HistoryField::ReasoningContent])
        );
        assert_eq!(
            refusal(400, exact.as_bytes()),
            Some(vec![HistoryField::ReasoningContent])
        );
        let wrong_role = wire.replace("assistant", "user");
        assert!(super::refusal(422, exact.as_bytes(), wrong_role.as_bytes()).is_none());
        let missing_index = json!({"messages":[]}).to_string();
        assert!(super::refusal(422, exact.as_bytes(), missing_index.as_bytes()).is_none());
        for status in [200, 401, 403, 429, 500] {
            assert!(refusal(status, exact.as_bytes()).is_none());
        }
        for loc in [
            json!(["body", "tools", 0, "reasoning_content"]),
            json!(["body", "messages", 0, "tool_calls", 0, "reasoning_content"]),
            json!(["body", "messages", 0, "user", "reasoning_content"]),
            json!(["body", "messages", 0, "assistant", "max_tokens"]),
        ] {
            assert!(refusal(422, error(loc, "extra_forbidden").as_bytes()).is_none());
        }
        assert!(
            refusal(
                422,
                error(
                    json!(["body", "messages", 0, "reasoning_content"]),
                    "missing"
                )
                .as_bytes()
            )
            .is_none()
        );
        assert!(
            refusal(
                422,
                br#"{"error":{"message":"reasoning_content unsupported"}}"#
            )
            .is_none()
        );
        let mixed = json!({"detail":[{"type":"extra_forbidden","loc":["messages",0,"reasoning"]},{"type":"missing","loc":["model"]}]}).to_string();
        assert!(refusal(422, mixed.as_bytes()).is_none());
    }

    #[test]
    fn omission_keeps_tools_alternate_fields_and_signed_history() {
        let body = json!({"model":"m","unknown":42,"messages":[
            {"role":"assistant","content":null,"reasoning_content":"secret","reasoning":"keep","tool_calls":[{"id":"t1","type":"function","function":{"name":"f","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"t1","content":"result"},
            {"role":"user","content":"next","reasoning_content":"user extension"}
        ]});
        let stripped: Value = serde_json::from_slice(
            &omit(
                &serde_json::to_vec(&body).unwrap(),
                &[HistoryField::ReasoningContent],
            )
            .unwrap(),
        )
        .unwrap();
        assert!(stripped["messages"][0].get("reasoning_content").is_none());
        assert_eq!(stripped["messages"][0]["reasoning"], "keep");
        assert_eq!(
            stripped["messages"][0]["tool_calls"],
            body["messages"][0]["tool_calls"]
        );
        assert_eq!(stripped["messages"][1], body["messages"][1]);
        assert_eq!(stripped["messages"][2], body["messages"][2]);
        assert_eq!(stripped["unknown"], 42);
        let signed = json!({"messages":[{"role":"assistant","reasoning_content":"thought","content":[{"type":"thinking","signature":"opaque","thinking":[]}]}]});
        assert!(
            omit(
                signed.to_string().as_bytes(),
                &[HistoryField::ReasoningContent]
            )
            .is_none()
        );
    }

    #[test]
    fn bounded_expiring_cache_is_isolated_by_account_model_api_and_mode() {
        let p = tw_config::Provider {
            name: "relay".into(),
            base_url: "https://unknown.example/v1".into(),
            ..Default::default()
        };
        let key = Key::of(&p, "m", "/v1/chat/completions", br#"{}"#).unwrap();
        let cache = Cache::default();
        let mut pending = cache.policy(&key, 1);
        pending.fields.push(HistoryField::ReasoningContent);
        assert!(cache.entries_for("relay", 1) == 0);
        cache.accepted(key.clone(), pending.clone(), 2);
        assert_eq!(cache.policy(&key, 3).fields, pending.fields);
        for different in [
            Key::of(&p, "other", "/v1/chat/completions", br#"{}"#).unwrap(),
            Key::of(&p, "m", "/chat/completions", br#"{}"#).unwrap(),
            Key::of(
                &p,
                "m",
                "/v1/chat/completions",
                br#"{"tools":[{"type":"function"}]}"#,
            )
            .unwrap(),
            Key::of(
                &p,
                "m",
                "/v1/chat/completions",
                br#"{"reasoning_effort":"high"}"#,
            )
            .unwrap(),
        ] {
            assert!(cache.policy(&different, 3).fields.is_empty());
        }
        let mut other_p = p.clone();
        other_p.key = Some("new-account".into());
        assert_ne!(
            key,
            Key::of(&other_p, "m", "/v1/chat/completions", br#"{}"#).unwrap()
        );
        let mut pending = cache.policy(&key, 5);
        pending.fields.push(HistoryField::Reasoning);
        cache.accepted(key.clone(), pending, 5);
        assert!(cache.policy(&key, 5 + TTL_MS).fields.is_empty());
        for i in 0..MAX + 1 {
            let key = Key::of(&p, &format!("m{i}"), "/v1/chat/completions", br#"{}"#).unwrap();
            let mut policy = cache.policy(&key, 10);
            policy.fields.push(HistoryField::Reasoning);
            cache.accepted(key, policy, 10 + i as u64);
        }
        assert_eq!(cache.entries_for("relay", MAX as u64 + 10), MAX);
    }
}
