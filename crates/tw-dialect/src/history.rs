//! Chat-compatible APIs share an envelope, not a reasoning-history schema.
//!
//! Keep the replay format and native thinking block in the existing signature
//! carrier. Old `tw1.ch.` histories remain readable. The target's capability
//! decides what can be replayed; opaque signatures never change formats or
//! cross a known source boundary.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ir::{Part, Request, Signature, Vendor};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HistoryFormat {
    #[default]
    Auto,
    ReasoningContent,
    Reasoning,
    ContentBlocks,
    Disabled,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryTarget {
    pub format: HistoryFormat,
    /// An upstream/credential compatibility group, never an API key.
    pub origin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatHistory {
    version: u8,
    pub format: HistoryFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

impl ChatHistory {
    pub fn field(format: HistoryFormat) -> Self {
        Self {
            version: 1,
            format,
            block: None,
            origin: None,
        }
    }

    pub fn content(block: &Value) -> Self {
        // Replay request fields only, not response-only metadata.
        let mut native = json!({"type": "thinking", "thinking": block.get("thinking").filter(|v| v.is_array()).cloned().unwrap_or_else(|| json!([]))});
        if let Some(signature) = block.get("signature").and_then(Value::as_str) {
            native["signature"] = json!(signature);
        }
        if let Some(closed) = block.get("closed").and_then(Value::as_bool) {
            native["closed"] = json!(closed);
        }
        Self {
            block: Some(native),
            ..Self::field(HistoryFormat::ContentBlocks)
        }
    }

    pub fn read(signature: &Signature) -> Option<Self> {
        if signature.vendor != Vendor::Chat {
            return None;
        }
        if signature.value.is_empty() {
            return Some(Self::field(HistoryFormat::ReasoningContent));
        }
        let history: Self = serde_json::from_str(&signature.value).ok()?;
        let valid_block = match history.format {
            HistoryFormat::ContentBlocks => history.block.as_ref().is_some_and(|b| {
                b.get("type").and_then(Value::as_str) == Some("thinking")
                    && b.get("thinking").is_some_and(Value::is_array)
            }),
            _ => history.block.is_none(),
        };
        (history.version == 1
            && valid_block
            && matches!(
                history.format,
                HistoryFormat::ReasoningContent
                    | HistoryFormat::Reasoning
                    | HistoryFormat::ContentBlocks
            ))
        .then_some(history)
    }

    pub fn signature(&self) -> Signature {
        if self.format == HistoryFormat::ReasoningContent
            && self.block.is_none()
            && self.origin.is_none()
        {
            return Signature::new(Vendor::Chat, "");
        }
        Signature::new(
            Vendor::Chat,
            serde_json::to_string(self).expect("reasoning history is JSON"),
        )
    }

    pub fn opaque(&self) -> bool {
        self.block
            .as_ref()
            .and_then(|b| b.get("signature"))
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    }

    /// A stream carries fragments of a native block, not complete messages.
    pub fn append(&mut self, next: Self) {
        let (Some(block), Some(next)) = (&mut self.block, next.block) else {
            return;
        };
        if let Some(parts) = next.get("thinking").and_then(Value::as_array) {
            let items = block["thinking"]
                .as_array_mut()
                .expect("native thinking is an array");
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str)
                    && part.get("type").and_then(Value::as_str) == Some("text")
                    && part.as_object().is_some_and(|o| o.len() == 2)
                    && let Some(last) = items.last_mut()
                    && last.get("type").and_then(Value::as_str) == Some("text")
                    && last.as_object().is_some_and(|o| o.len() == 2)
                    && let Some(Value::String(previous)) = last.get_mut("text")
                {
                    previous.push_str(text);
                    continue;
                }
                items.push(part.clone());
            }
        }
        for key in ["signature", "closed"] {
            if let Some(value) = next.get(key) {
                block[key] = value.clone();
            }
        }
    }

    pub fn format_for(&self, target: &HistoryTarget) -> HistoryFormat {
        match target.format {
            // A carried native shape belongs to its source. Switching to an
            // unknown upstream does not establish request-field compatibility.
            HistoryFormat::Auto if self.origin.is_some() && self.origin != target.origin => {
                HistoryFormat::Disabled
            }
            // A response's `reasoning` string does not establish that the
            // request accepts the same field. Keep the existing compatible
            // request convention unless a capability explicitly selects it.
            HistoryFormat::Auto if self.format == HistoryFormat::Reasoning => {
                HistoryFormat::ReasoningContent
            }
            HistoryFormat::Auto => self.format,
            format => format,
        }
    }

    pub fn compatible(&self, target: &HistoryTarget) -> bool {
        !self.opaque()
            || (self.format_for(target) == self.format
                && (self.origin.is_none() || self.origin == target.origin))
    }

    pub fn stamp(signature: &mut Signature, origin: Option<&str>) {
        if let Some(origin) = origin
            && let Some(mut history) = Self::read(signature)
        {
            history.origin = Some(origin.to_string());
            *signature = history.signature();
        }
    }
}

pub fn incompatible(request: &Request, target: &HistoryTarget, chat: bool) -> bool {
    request.messages.iter().flat_map(|m| &m.parts).any(|part| {
        let Part::Thinking(thinking) = part else {
            return false;
        };
        thinking.signature.as_ref().is_some_and(|signature| {
            if signature.vendor != Vendor::Chat {
                return false;
            }
            match ChatHistory::read(signature) {
                Some(history) => history.opaque() && (!chat || !history.compatible(target)),
                None => !signature.value.is_empty(),
            }
        })
    })
}

/// Apply an explicit capability to a native Chat request without round-tripping
/// unrelated fields through the IR. Auto leaves native requests byte-for-byte.
pub fn shape_chat(body: &[u8], format: HistoryFormat) -> Option<Vec<u8>> {
    if format == HistoryFormat::Auto
        || ![
            b"reasoning_content".as_slice(),
            b"\"reasoning\"",
            b"\"thinking\"",
            b"\\u",
        ]
        .iter()
        .any(|needle| memchr::memmem::find(body, needle).is_some())
    {
        return None;
    }
    let mut request: Value = serde_json::from_slice(body).ok()?;
    let original = request.clone();
    let mut changed = false;
    let messages = request.get_mut("messages")?.as_array_mut()?;
    let mut empty_messages = Vec::new();
    for (index, message) in messages.iter_mut().enumerate() {
        let mut message_changed = false;
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(object) = message.as_object_mut() else {
            continue;
        };
        let mut thoughts = Vec::new();
        for key in ["reasoning_content", "reasoning"] {
            if let Some(value) = object.remove(key) {
                changed = true;
                message_changed = true;
                if let Some(text) = value.as_str().filter(|s| !s.is_empty()) {
                    thoughts.push(text.to_string());
                }
            }
        }
        let content = object.get("content").cloned().unwrap_or(Value::Null);
        let mut blocks = match &content {
            Value::Array(parts) => parts.clone(),
            Value::String(text) if !text.is_empty() => vec![json!({"type": "text", "text": text})],
            _ => Vec::new(),
        };
        match format {
            HistoryFormat::ContentBlocks => {
                if !thoughts.is_empty() {
                    blocks.insert(0, json!({"type": "thinking", "thinking": [{"type": "text", "text": thoughts.join("\n\n")}]}));
                    object.insert("content".into(), json!(blocks));
                }
            }
            HistoryFormat::Reasoning
            | HistoryFormat::ReasoningContent
            | HistoryFormat::Disabled => {
                let before = blocks.len();
                blocks.retain(|block| {
                    if block.get("type").and_then(Value::as_str) != Some("thinking") {
                        return true;
                    }
                    if let Some(parts) = block.get("thinking").and_then(Value::as_array) {
                        thoughts.push(
                            parts
                                .iter()
                                .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                                .filter_map(|p| p.get("text").and_then(Value::as_str))
                                .collect::<String>(),
                        );
                    }
                    false
                });
                if before != blocks.len() {
                    changed = true;
                    message_changed = true;
                    object.insert(
                        "content".into(),
                        if blocks.is_empty() {
                            Value::Null
                        } else {
                            json!(blocks)
                        },
                    );
                }
                if format != HistoryFormat::Disabled && !thoughts.is_empty() {
                    object.insert(
                        if format == HistoryFormat::Reasoning {
                            "reasoning"
                        } else {
                            "reasoning_content"
                        }
                        .into(),
                        json!(thoughts.join("\n\n")),
                    );
                }
            }
            HistoryFormat::Auto => {}
        }
        let empty_content = object.get("content").is_none_or(|v| {
            v.is_null() || v.as_str() == Some("") || v.as_array().is_some_and(Vec::is_empty)
        });
        let has_other_content = [
            "tool_calls",
            "function_call",
            "audio",
            "refusal",
            "reasoning_content",
            "reasoning",
        ]
        .iter()
        .any(|key| {
            object.get(*key).is_some_and(|v| {
                !v.is_null() && v.as_str() != Some("") && !v.as_array().is_some_and(Vec::is_empty)
            })
        });
        if message_changed && empty_content && !has_other_content {
            empty_messages.push(index);
        }
    }
    let mut index = 0;
    messages.retain(|_| {
        let keep = !empty_messages.contains(&index);
        index += 1;
        keep
    });
    (changed && request != original).then(|| request.to_string().into_bytes())
}
