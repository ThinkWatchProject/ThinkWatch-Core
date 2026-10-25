//! 请求参数的读写：同一个参数在各格式里叫不同的名字、放在不同的地方。
//!
//! 「最大输出」在 Anthropic 是 `max_tokens`，Chat 是 `max_completion_tokens` 或
//! `max_tokens`，Responses 是 `max_output_tokens`，Gemini 在 `generationConfig` 里叫
//! `maxOutputTokens`，Bedrock 在 `inferenceConfig` 里叫 `maxTokens`。改写请求参数的
//! 地方（桌面版路由规则的 `set`、企业版按模型限制最大输出）都从这里读写，名字只在
//! 这里写一遍。读的判据和解码器一致（同一个请求，这里读出来的数和中间表示里的
//! `max_tokens` 一样）。
//!
//! **改的是原文**（[`serde_json::Value`]），不经过中间表示：同格式直通的请求要保住中间
//! 表示不装的那些字段。没写这个参数、要写进去时，写在这种格式通常写的位置上。请求体
//! 不是 JSON 对象的，读是 `None`，写什么都不做。

use serde_json::{Map, Value};

use crate::ir::{Dialect, Effort};
use crate::think;

/// 客户端写的最大输出 token 数。没写（或者写的不是一个非负整数）是 `None`。
pub fn max_output_tokens(dialect: Dialect, body: &Value) -> Option<u64> {
    let u = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64);
    match dialect {
        Dialect::Anthropic => u(body, "max_tokens"),
        // 两个都写了的，解码器先认 max_completion_tokens
        Dialect::Chat => u(body, "max_completion_tokens").or_else(|| u(body, "max_tokens")),
        Dialect::Responses => u(body, "max_output_tokens"),
        // 和解码器一样：驼峰的在就认驼峰的，否则认下划线的
        Dialect::Gemini => either(
            either(body, "generationConfig", "generation_config")?,
            "maxOutputTokens",
            "max_output_tokens",
        )?
        .as_u64(),
        Dialect::Bedrock => body.get("inferenceConfig").and_then(|c| u(c, "maxTokens")),
    }
}

/// 把最大输出 token 数写成 `n`。
///
/// Chat 两个名字都可能写：客户端写了哪个（或者两个都写了）就改哪个，都没写时写
/// `max_tokens` —— OpenAI 官方的推理模型只认 `max_completion_tokens`，但它们的客户端
/// 本来就那么写；兼容实现大多只认 `max_tokens`。Gemini 的 `generationConfig` 客户端写成
/// 下划线（`generation_config`）的，写进它里面，不另起一个驼峰的。
pub fn set_max_output_tokens(dialect: Dialect, body: &mut Value, n: u64) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let n = Value::from(n);
    match dialect {
        Dialect::Anthropic => {
            obj.insert("max_tokens".into(), n);
        }
        Dialect::Chat => {
            let written: Vec<&str> = ["max_completion_tokens", "max_tokens"]
                .into_iter()
                .filter(|k| obj.contains_key(*k))
                .collect();
            if written.is_empty() {
                obj.insert("max_tokens".into(), n);
            } else {
                for k in written {
                    obj.insert(k.into(), n.clone());
                }
            }
        }
        Dialect::Responses => {
            obj.insert("max_output_tokens".into(), n);
        }
        Dialect::Gemini => {
            // 已有的生成参数里写，客户端写过哪个名字改哪个；没有就新建一个驼峰的
            let config = gemini_config_key(obj).unwrap_or("generationConfig");
            if let Some(g) = section(obj, config) {
                let field = gemini_max_key(g).unwrap_or(if config == "generation_config" {
                    "max_output_tokens"
                } else {
                    "maxOutputTokens"
                });
                g.insert(field.into(), n);
            }
        }
        Dialect::Bedrock => {
            if let Some(c) = section(obj, "inferenceConfig") {
                c.insert("maxTokens".into(), n);
            }
        }
    }
}

/// 把最大输出 token 数限制在 `cap` 以内：客户端写的比它大、或者没写，就写成 `cap`；
/// 写的不比它大就不动。返回改没改。`official` 是这个请求要发往的是不是厂商官方的端点
/// （[`crate::official::is_official_host`]，和转换时的 [`crate::ir::Target::official`]
/// 一个意思）。
///
/// 和 [`set_max_output_tokens`] 不一样的几处，都是因为「限制」要对上游真的管用：
///
/// - **Chat 两个名字都写了的，各自压到 `cap` 以内。**只看解码器先认的那一个
///   （`max_completion_tokens`）的话，`{"max_completion_tokens": 100, "max_tokens": 9999}`
///   原样过去，只认 `max_tokens` 的上游等于没限。比 `cap` 小的那个不动，不替客户端放宽。
///   值是 `null` 的算没写。
/// - **Chat 两个都没写时，官方端点写 `max_completion_tokens`，别家写 `max_tokens`**：
///   OpenAI 官方的推理模型不认 `max_tokens`，带着它整个请求 400；兼容实现大多只认
///   `max_tokens`。和编码器选名字的办法一样。
/// - **Anthropic 开了思考的**（`thinking.type` 是 `enabled`，带 `budget_tokens`）：思考
///   用的 token 算在 `max_tokens` 里，所以 `budget_tokens` 必须小于 `max_tokens`，否则整个
///   请求 400。把 `max_tokens` 压到不比预算大时，`cap` 大于 1024（预算的下限）就把预算一并
///   压到 `cap - 1`；不大于 1024 的话预算没有合法的值可取，去掉 `thinking` —— 不思考地
///   回答，总比整个请求被拒强。Converse（`additionalModelRequestFields.thinking`）背后的
///   Claude 同理。预算本来就比限制小的不动；客户端自己写的 `max_tokens` 就不比预算大的，
///   是它自己的请求不合法，也不动。
pub fn cap_max_output_tokens(dialect: Dialect, body: &mut Value, cap: u64, official: bool) -> bool {
    let Some(obj) = body.as_object_mut() else {
        return false;
    };
    if dialect == Dialect::Chat {
        return cap_chat(obj, cap, official);
    }
    if max_output_tokens(dialect, body).is_some_and(|n| n <= cap) {
        return false;
    }
    set_max_output_tokens(dialect, body, cap);
    let thinking = match dialect {
        Dialect::Anthropic => body.as_object_mut(),
        Dialect::Bedrock => body
            .get_mut("additionalModelRequestFields")
            .and_then(Value::as_object_mut),
        _ => None,
    };
    if let Some(holder) = thinking {
        fit_thinking(holder, cap);
    }
    true
}

/// Chat 的两个名字（见 [`cap_max_output_tokens`]）
fn cap_chat(obj: &mut Map<String, Value>, cap: u64, official: bool) -> bool {
    const NAMES: [&str; 2] = ["max_completion_tokens", "max_tokens"];
    let written: Vec<&str> = NAMES
        .into_iter()
        .filter(|k| obj.get(*k).is_some_and(|v| !v.is_null()))
        .collect();
    if written.is_empty() {
        let name = if official { NAMES[0] } else { NAMES[1] };
        obj.insert(name.into(), Value::from(cap));
        return true;
    }
    let mut changed = false;
    for k in written {
        // 写的不是一个非负整数的，和比上限大的一样换成上限：上游读不懂它，就等于没限
        if !obj.get(k).and_then(Value::as_u64).is_some_and(|n| n <= cap) {
            obj.insert(k.into(), Value::from(cap));
            changed = true;
        }
    }
    changed
}

/// `max_tokens` 刚压到 `cap` 之后，让思考的预算还小于它（见 [`cap_max_output_tokens`]）。
/// `holder` 是装着 `thinking` 的那个对象
fn fit_thinking(holder: &mut Map<String, Value>, cap: u64) {
    /// Anthropic 允许的最小思考预算
    const BUDGET_MIN: u64 = 1024;
    let Some(thinking) = holder.get_mut("thinking").and_then(Value::as_object_mut) else {
        return;
    };
    if thinking.get("type").and_then(Value::as_str) != Some("enabled") {
        return;
    }
    let Some(budget) = thinking.get("budget_tokens").and_then(Value::as_u64) else {
        return;
    };
    if budget < cap {
        return;
    }
    if cap > BUDGET_MIN {
        thinking.insert("budget_tokens".into(), Value::from(cap - 1));
    } else {
        holder.remove("thinking");
    }
}

/// 改要的模型。Gemini 和 Bedrock 的模型写在路径里，请求体里没有它，什么都不做（路径
/// 由调用方改）。
pub fn set_model(dialect: Dialect, body: &mut Value, model: &str) {
    if matches!(dialect, Dialect::Gemini | Dialect::Bedrock) {
        return;
    }
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".into(), Value::String(model.to_string()));
    }
}

/// 关掉推理：去掉开推理的那个字段，连同推理强度。
///
/// **只做「关掉」这一个方向**：开启要一个预算，而这一层没有一个说得过去的值可以编。
/// Anthropic 的强度（`output_config.effort`）单独写在思考开关外面，只去掉开关的话自适应的
/// 模型照样按那一档想，要一起去掉；`output_config` 去空了的整个去掉。Converse 的透传口袋里
/// 同理。
pub fn disable_thinking(dialect: Dialect, body: &mut Value) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    match dialect {
        Dialect::Anthropic => {
            obj.remove("thinking");
            remove_output_effort(obj);
        }
        Dialect::Chat => {
            obj.remove("reasoning_effort");
        }
        Dialect::Responses => {
            obj.remove("reasoning");
        }
        Dialect::Gemini => {
            for config in ["generationConfig", "generation_config"] {
                if let Some(g) = obj.get_mut(config).and_then(Value::as_object_mut) {
                    g.remove("thinkingConfig");
                    g.remove("thinking_config");
                }
            }
        }
        // Converse 没有一等公民的思考开关，它在透传口袋里
        Dialect::Bedrock => {
            if let Some(f) = obj
                .get_mut("additionalModelRequestFields")
                .and_then(Value::as_object_mut)
            {
                f.remove("thinking");
                remove_output_effort(f);
            }
        }
    }
}

/// 去掉 `output_config.effort`；`output_config` 去空了的整个去掉。返回去没去掉
fn remove_output_effort(holder: &mut Map<String, Value>) -> bool {
    let Some(o) = holder
        .get_mut("output_config")
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    let removed = o.remove("effort").is_some();
    if o.is_empty() {
        holder.remove("output_config");
    }
    removed
}

// ───────────────────────────────────────────────────────── 推理强度
//
// 「推理开着」和解码器的判据一样：Anthropic 的 `thinking.type` 是 `enabled` / `adaptive`，或者
// 只写了 `output_config.effort`；Chat 的 `reasoning_effort` 不是 `none`，或者 GLM 那种
// `thinking.type: enabled`；Responses 写了 `reasoning`、强度不是 `none`（对话中途的
// `configuration_update` 最后一个说了算）；Gemini 写了 `thinkingConfig`、预算不是 0；Converse
// 的透传口袋里和 Anthropic 一样。明确关掉的（`disabled`、`none`、预算 0）不算开着。
//
// 强度写在哪儿：Anthropic 是 `output_config.effort`，按预算思考的（`enabled` +
// `budget_tokens`、没写强度）是预算；Chat 是 `reasoning_effort`；Responses 是
// `reasoning.effort`；Gemini 写了等级的是 `thinkingLevel`、写了预算的是 `thinkingBudget`；
// Converse 和 Anthropic 一样，在 `additionalModelRequestFields` 里。预算按
// [`think::effort_of_budget`] 读成一档、按 [`think::budget_of_effort`] 写回去。

/// 客户端开着推理吗（判据见上）
pub fn reasoning_on(dialect: Dialect, body: &Value) -> bool {
    let Some(obj) = body.as_object() else {
        return false;
    };
    match dialect {
        Dialect::Anthropic => claude_on(obj),
        // 和解码器一样：`thinking` 说关掉的以它为准，`reasoning_effort` 认得出的看它，都没有再看
        // `thinking` 是不是开着
        Dialect::Chat => {
            let switch = obj
                .get("thinking")
                .and_then(|t| t.get("type"))
                .and_then(Value::as_str);
            match str_at(obj, "reasoning_effort").and_then(think::parse_openai) {
                _ if switch == Some("disabled") => false,
                Some(e) => e.is_some(),
                None => switch == Some("enabled"),
            }
        }
        Dialect::Responses => match responses_update(obj).and_then(think::parse_openai) {
            Some(e) => e.is_some(),
            None => obj
                .get("reasoning")
                .filter(|r| r.is_object())
                .is_some_and(|r| r.get("effort").and_then(Value::as_str) != Some("none")),
        },
        Dialect::Gemini => gemini_thinking(obj).is_some_and(|t| {
            either_ref(t, "thinkingBudget", "thinking_budget").and_then(Value::as_i64) != Some(0)
        }),
        Dialect::Bedrock => obj
            .get("additionalModelRequestFields")
            .and_then(Value::as_object)
            .is_some_and(claude_on),
    }
}

/// 客户端要的推理强度。推理没开、开着却没说强度（自适应的、Gemini 让模型自己定的）是 `None`
pub fn reasoning_effort(dialect: Dialect, body: &Value) -> Option<Effort> {
    if !reasoning_on(dialect, body) {
        return None;
    }
    let obj = body.as_object()?;
    match dialect {
        Dialect::Anthropic => claude_effort(obj),
        Dialect::Chat => str_at(obj, "reasoning_effort")
            .and_then(think::parse_openai)
            .flatten(),
        Dialect::Responses => responses_update(obj)
            .or_else(|| obj.get("reasoning").and_then(|r| r.get("effort"))?.as_str())
            .and_then(think::parse_openai)
            .flatten(),
        Dialect::Gemini => {
            let t = gemini_thinking(obj)?;
            match either_ref(t, "thinkingLevel", "thinking_level").and_then(Value::as_str) {
                Some(l) => think::parse_gemini_level(l),
                None => either_ref(t, "thinkingBudget", "thinking_budget")
                    .and_then(Value::as_i64)
                    .filter(|b| *b > 0)
                    .map(|b| think::effort_of_budget(b as u64)),
            }
        }
        Dialect::Bedrock => obj
            .get("additionalModelRequestFields")
            .and_then(Value::as_object)
            .and_then(claude_effort),
    }
}

/// 把推理强度改成 `e`。**只改开着推理的请求**：没开的（生成标题这类辅助请求、明确关掉的）
/// 原样不动，不替客户端打开推理。返回改没改。
///
/// 客户端写了强度的改强度；按预算思考的改预算（Anthropic 和 Converse 的预算还要小于最大输出，
/// 压不到 1024 以上的不改）；Gemini 等级、预算都没写的（让模型自己定）不改 —— 不知道这个模型
/// 认哪一个。Anthropic 自适应、没写强度的补上 `output_config.effort`。Responses 对话中途的
/// `configuration_update` 一起改：最后一个盖过顶层的。
pub fn set_reasoning_effort(dialect: Dialect, body: &mut Value, e: Effort) -> bool {
    if !reasoning_on(dialect, body) {
        return false;
    }
    let Some(obj) = body.as_object_mut() else {
        return false;
    };
    match dialect {
        Dialect::Anthropic => {
            let max = obj.get("max_tokens").and_then(Value::as_u64);
            set_claude_effort(obj, e, max)
        }
        Dialect::Chat => put(obj, "reasoning_effort", think::openai(e)),
        Dialect::Responses => {
            let mut changed = match obj.get_mut("reasoning") {
                Some(Value::Object(r)) => put(r, "effort", think::openai(e)),
                Some(_) => false,
                None => {
                    obj.insert("reasoning".into(), serde_json::json!({}));
                    match obj.get_mut("reasoning") {
                        Some(Value::Object(r)) => put(r, "effort", think::openai(e)),
                        _ => false,
                    }
                }
            };
            for r in responses_updates(obj) {
                if r.contains_key("effort") {
                    changed |= put(r, "effort", think::openai(e));
                }
            }
            changed
        }
        Dialect::Gemini => {
            let Some(t) = gemini_thinking_mut(obj) else {
                return false;
            };
            if let Some(k) = key_of(t, "thinkingLevel", "thinking_level") {
                put(t, k, think::gemini_level(e))
            } else if let Some(k) = key_of(t, "thinkingBudget", "thinking_budget") {
                let budget = think::budget_of_effort(e);
                let before = t.get(k).cloned();
                t.insert(k.into(), Value::from(budget));
                before.as_ref() != t.get(k)
            } else {
                false
            }
        }
        Dialect::Bedrock => {
            let max = obj
                .get("inferenceConfig")
                .and_then(|c| c.get("maxTokens"))
                .and_then(Value::as_u64);
            match obj
                .get_mut("additionalModelRequestFields")
                .and_then(Value::as_object_mut)
            {
                Some(f) => set_claude_effort(f, e, max),
                None => false,
            }
        }
    }
}

/// 去掉写明的推理强度，按模型的默认想。推理照样开着。返回去没去掉。
///
/// 按预算思考的 Anthropic 和 Converse 不动：预算是开思考必须的，去掉它整个请求不合法。
/// Responses 对话中途的 `configuration_update` 里的也去掉，只剩个空壳的整条去掉。
pub fn remove_reasoning_effort(dialect: Dialect, body: &mut Value) -> bool {
    if !reasoning_on(dialect, body) {
        return false;
    }
    let Some(obj) = body.as_object_mut() else {
        return false;
    };
    match dialect {
        Dialect::Anthropic => remove_output_effort(obj),
        Dialect::Chat => obj.remove("reasoning_effort").is_some(),
        Dialect::Responses => {
            let mut removed = obj
                .get_mut("reasoning")
                .and_then(Value::as_object_mut)
                .is_some_and(|r| r.remove("effort").is_some());
            if let Some(Value::Array(items)) = obj.get_mut("input") {
                let before = items.len();
                for item in items.iter_mut() {
                    if let Some(r) = update_reasoning(item) {
                        removed |= r.remove("effort").is_some();
                    }
                }
                // 只剩 `type`（和 `id`）的一条没有意思了
                items.retain(|item| {
                    !(is_update(item)
                        && item
                            .get("reasoning")
                            .is_some_and(|r| r.as_object().is_some_and(Map::is_empty))
                        && item.as_object().is_some_and(|o| {
                            o.keys()
                                .all(|k| matches!(k.as_str(), "type" | "id" | "reasoning"))
                        }))
                });
                removed |= items.len() != before;
            }
            removed
        }
        Dialect::Gemini => {
            let Some(t) = gemini_thinking_mut(obj) else {
                return false;
            };
            let mut removed = false;
            for k in ["thinkingLevel", "thinking_level"] {
                removed |= t.remove(k).is_some();
            }
            for k in ["thinkingBudget", "thinking_budget"] {
                if t.get(k).and_then(Value::as_i64).is_some_and(|b| b > 0) {
                    removed |= t.remove(k).is_some();
                }
            }
            removed
        }
        Dialect::Bedrock => obj
            .get_mut("additionalModelRequestFields")
            .and_then(Value::as_object_mut)
            .is_some_and(remove_output_effort),
    }
}

/// Anthropic（和 Converse 背后的 Claude）开着思考吗。`holder` 是装着 `thinking` 的那个对象
fn claude_on(holder: &Map<String, Value>) -> bool {
    match holder
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(Value::as_str)
    {
        Some("enabled" | "adaptive") => true,
        Some(_) => false,
        None => output_effort(holder).is_some(),
    }
}

fn output_effort(holder: &Map<String, Value>) -> Option<Effort> {
    holder
        .get("output_config")
        .and_then(|o| o.get("effort"))
        .and_then(Value::as_str)
        .and_then(think::parse_anthropic)
}

/// 按预算思考的预算。`thinking.type` 是 `enabled` 才有
fn claude_budget(holder: &Map<String, Value>) -> Option<u64> {
    let t = holder.get("thinking")?;
    (t.get("type").and_then(Value::as_str) == Some("enabled"))
        .then(|| t.get("budget_tokens").and_then(Value::as_u64))
        .flatten()
}

fn claude_effort(holder: &Map<String, Value>) -> Option<Effort> {
    output_effort(holder).or_else(|| claude_budget(holder).map(think::effort_of_budget))
}

/// 见 [`set_reasoning_effort`]。`max` 是最大输出：预算要小于它
fn set_claude_effort(holder: &mut Map<String, Value>, e: Effort, max: Option<u64>) -> bool {
    /// Anthropic 允许的最小思考预算
    const BUDGET_MIN: u64 = 1024;
    let effort_written = holder
        .get("output_config")
        .is_some_and(|o| o.get("effort").is_some());
    if !effort_written && let Some(before) = claude_budget(holder) {
        let budget = max.map_or(think::budget_of_effort(e), |m| {
            think::budget_of_effort(e).min(m.saturating_sub(1))
        });
        if budget < BUDGET_MIN || budget == before {
            return false;
        }
        if let Some(t) = holder.get_mut("thinking").and_then(Value::as_object_mut) {
            t.insert("budget_tokens".into(), Value::from(budget));
            return true;
        }
        return false;
    }
    match section(holder, "output_config") {
        Some(o) => put(o, "effort", think::anthropic(e)),
        None => false,
    }
}

/// 把 `obj[key]` 写成字符串 `value`。返回值变没变
fn put(obj: &mut Map<String, Value>, key: &str, value: &str) -> bool {
    if obj.get(key).and_then(Value::as_str) == Some(value) {
        return false;
    }
    obj.insert(key.into(), Value::String(value.to_string()));
    true
}

fn str_at<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(Value::as_str)
}

fn is_update(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("configuration_update")
}

/// 一条 `configuration_update` 里的 `reasoning`
fn update_reasoning(item: &mut Value) -> Option<&mut Map<String, Value>> {
    if !is_update(item) {
        return None;
    }
    item.get_mut("reasoning").and_then(Value::as_object_mut)
}

/// Responses 对话中途最后一次改的推理强度（`input` 里最后一条带着强度的
/// `configuration_update`），和解码器一样它盖过顶层的
fn responses_update(obj: &Map<String, Value>) -> Option<&str> {
    obj.get("input")?
        .as_array()?
        .iter()
        .filter(|i| is_update(i))
        .filter_map(|i| i.get("reasoning")?.get("effort")?.as_str())
        .next_back()
}

/// 每一条 `configuration_update` 里的 `reasoning`
fn responses_updates(obj: &mut Map<String, Value>) -> Vec<&mut Map<String, Value>> {
    match obj.get_mut("input") {
        Some(Value::Array(items)) => items.iter_mut().filter_map(update_reasoning).collect(),
        _ => Vec::new(),
    }
}

/// Gemini 的思考配置：生成参数里的 `thinkingConfig`，两层都认下划线的写法（先认驼峰）
fn gemini_thinking(obj: &Map<String, Value>) -> Option<&Map<String, Value>> {
    let g = either_ref(obj, "generationConfig", "generation_config")?;
    either(g, "thinkingConfig", "thinking_config")?.as_object()
}

fn gemini_thinking_mut(obj: &mut Map<String, Value>) -> Option<&mut Map<String, Value>> {
    let g = key_of(obj, "generationConfig", "generation_config")?;
    let g = obj.get_mut(g)?.as_object_mut()?;
    let t = key_of(g, "thinkingConfig", "thinking_config")?;
    g.get_mut(t)?.as_object_mut()
}

/// [`either`] 的对象版
fn either_ref<'a>(obj: &'a Map<String, Value>, camel: &str, snake: &str) -> Option<&'a Value> {
    obj.get(camel).or_else(|| obj.get(snake))
}

/// 两个写法里客户端用的那一个键，先认驼峰
fn key_of<'k>(obj: &Map<String, Value>, camel: &'k str, snake: &'k str) -> Option<&'k str> {
    [camel, snake].into_iter().find(|k| obj.contains_key(*k))
}

/// Gemini 的一个字段：驼峰的在就是它，否则是下划线写法的
fn either<'a>(v: &'a Value, camel: &str, snake: &str) -> Option<&'a Value> {
    v.get(camel).or_else(|| v.get(snake))
}

/// Gemini 的生成参数写在哪个键下：客户端写了哪个用哪个，先认驼峰（和解码器一样）
fn gemini_config_key(body: &Map<String, Value>) -> Option<&'static str> {
    ["generationConfig", "generation_config"]
        .into_iter()
        .find(|k| body.get(*k).is_some_and(Value::is_object))
}

/// 生成参数里最大输出写在哪个键下，先认驼峰
fn gemini_max_key(g: &Map<String, Value>) -> Option<&'static str> {
    ["maxOutputTokens", "max_output_tokens"]
        .into_iter()
        .find(|k| g.contains_key(*k))
}

/// `obj` 里名叫 `key` 的那个对象，没有就新建一个空的。那个键上写的不是对象的，`None`
fn section<'a>(obj: &'a mut Map<String, Value>, key: &str) -> Option<&'a mut Map<String, Value>> {
    obj.entry(key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ALL: [Dialect; 5] = [
        Dialect::Anthropic,
        Dialect::Chat,
        Dialect::Responses,
        Dialect::Gemini,
        Dialect::Bedrock,
    ];

    #[test]
    fn what_is_written_is_what_is_read_back_in_every_format() {
        for d in ALL {
            let mut v = json!({});
            assert_eq!(max_output_tokens(d, &v), None, "{d:?}");
            set_max_output_tokens(d, &mut v, 512);
            assert_eq!(max_output_tokens(d, &v), Some(512), "{d:?}: {v}");
        }
    }

    #[test]
    fn each_format_keeps_its_own_field_names() {
        let set = |d, mut v: Value| {
            set_max_output_tokens(d, &mut v, 7);
            v
        };
        assert_eq!(set(Dialect::Anthropic, json!({})), json!({"max_tokens": 7}));
        assert_eq!(set(Dialect::Chat, json!({})), json!({"max_tokens": 7}));
        assert_eq!(
            set(Dialect::Chat, json!({"max_completion_tokens": 9})),
            json!({"max_completion_tokens": 7})
        );
        assert_eq!(
            set(
                Dialect::Chat,
                json!({"max_completion_tokens": 9, "max_tokens": 9})
            ),
            json!({"max_completion_tokens": 7, "max_tokens": 7}),
            "两个都写了的两个都改，否则上游按哪一个都不一定"
        );
        assert_eq!(
            set(Dialect::Responses, json!({})),
            json!({"max_output_tokens": 7})
        );
        assert_eq!(
            set(
                Dialect::Gemini,
                json!({"generationConfig": {"temperature": 0}})
            ),
            json!({"generationConfig": {"temperature": 0, "maxOutputTokens": 7}})
        );
        assert_eq!(
            set(Dialect::Bedrock, json!({})),
            json!({"inferenceConfig": {"maxTokens": 7}})
        );
    }

    #[test]
    fn gemini_written_in_snake_case_stays_one_object() {
        // 另起一个驼峰的 generationConfig 的话，请求里就有两份生成参数
        let mut v = json!({"generation_config": {"max_output_tokens": 9, "top_k": 3}});
        assert_eq!(max_output_tokens(Dialect::Gemini, &v), Some(9));
        set_max_output_tokens(Dialect::Gemini, &mut v, 7);
        assert_eq!(
            v,
            json!({"generation_config": {"max_output_tokens": 7, "top_k": 3}})
        );
        let mut v = json!({"generation_config": {}});
        set_max_output_tokens(Dialect::Gemini, &mut v, 7);
        assert_eq!(v, json!({"generation_config": {"max_output_tokens": 7}}));
    }

    #[test]
    fn reading_agrees_with_the_decoder() {
        // 同一个请求，这里读出来的和中间表示里的一样
        let cases = [
            (
                Dialect::Chat,
                json!({"max_completion_tokens": 5, "max_tokens": 9, "messages": []}),
            ),
            (
                Dialect::Gemini,
                json!({"generation_config": {"max_output_tokens": 5}, "contents": []}),
            ),
            (
                Dialect::Bedrock,
                json!({"inferenceConfig": {"maxTokens": 5}, "messages": []}),
            ),
        ];
        for (d, v) in cases {
            let path = match d {
                Dialect::Gemini => "/v1beta/models/m:generateContent",
                Dialect::Bedrock => "/model/m/converse",
                _ => "/",
            };
            let ir = crate::convert::decode(d, &v, path, None).unwrap();
            assert_eq!(ir.request.max_tokens, max_output_tokens(d, &v), "{d:?}");
            assert_eq!(max_output_tokens(d, &v), Some(5));
        }
    }

    #[test]
    fn a_cap_lowers_or_fills_and_leaves_a_smaller_value_alone() {
        for official in [false, true] {
            for d in ALL {
                let mut v = json!({});
                assert!(cap_max_output_tokens(d, &mut v, 100, official), "{d:?}");
                assert_eq!(max_output_tokens(d, &v), Some(100));
                set_max_output_tokens(d, &mut v, 50);
                assert!(!cap_max_output_tokens(d, &mut v, 100, official), "{d:?}");
                assert_eq!(max_output_tokens(d, &v), Some(50));
                set_max_output_tokens(d, &mut v, 500);
                assert!(cap_max_output_tokens(d, &mut v, 100, official), "{d:?}");
                assert_eq!(max_output_tokens(d, &v), Some(100));
            }
        }
        let mut not_an_object = json!([1]);
        assert!(!cap_max_output_tokens(
            Dialect::Chat,
            &mut not_an_object,
            1,
            false
        ));
        assert_eq!(not_an_object, json!([1]));
    }

    /// 四种客户端格式（外加 Converse）各压一次：`(格式, 请求, 官方端点吗)` → 压完的样子
    fn capped(d: Dialect, mut v: Value, cap: u64, official: bool) -> (bool, Value) {
        let changed = cap_max_output_tokens(d, &mut v, cap, official);
        (changed, v)
    }

    #[test]
    fn a_chat_request_that_names_both_fields_is_capped_in_both() {
        // 只看先认的那一个的话，`max_tokens` 原样过去，只认它的上游等于没限
        let both = json!({"max_completion_tokens": 50, "max_tokens": 5000});
        for official in [false, true] {
            assert_eq!(
                capped(Dialect::Chat, both.clone(), 100, official),
                (
                    true,
                    json!({"max_completion_tokens": 50, "max_tokens": 100})
                )
            );
            // 反过来也一样；比上限小的那个不替客户端放宽
            assert_eq!(
                capped(
                    Dialect::Chat,
                    json!({"max_completion_tokens": 5000, "max_tokens": 50}),
                    100,
                    official
                ),
                (
                    true,
                    json!({"max_completion_tokens": 100, "max_tokens": 50})
                )
            );
            // 两个都不大：不动
            let small = json!({"max_completion_tokens": 50, "max_tokens": 60});
            assert_eq!(
                capped(Dialect::Chat, small.clone(), 100, official),
                (false, small)
            );
            // 读不懂的值等于没限，换成上限；`null` 算没写
            assert_eq!(
                capped(
                    Dialect::Chat,
                    json!({"max_completion_tokens": "lots", "max_tokens": 50}),
                    100,
                    official
                ),
                (
                    true,
                    json!({"max_completion_tokens": 100, "max_tokens": 50})
                )
            );
            assert_eq!(
                capped(
                    Dialect::Chat,
                    json!({"max_completion_tokens": null, "max_tokens": 50}),
                    100,
                    official
                ),
                (
                    false,
                    json!({"max_completion_tokens": null, "max_tokens": 50})
                )
            );
        }
        // 别的格式只有一个名字，照常压
        for (d, v, want) in [
            (
                Dialect::Anthropic,
                json!({"max_tokens": 5000}),
                json!({"max_tokens": 100}),
            ),
            (
                Dialect::Responses,
                json!({"max_output_tokens": 5000}),
                json!({"max_output_tokens": 100}),
            ),
            (
                Dialect::Gemini,
                json!({"generationConfig": {"maxOutputTokens": 5000}}),
                json!({"generationConfig": {"maxOutputTokens": 100}}),
            ),
            (
                Dialect::Bedrock,
                json!({"inferenceConfig": {"maxTokens": 5000}}),
                json!({"inferenceConfig": {"maxTokens": 100}}),
            ),
        ] {
            assert_eq!(capped(d, v, 100, false), (true, want), "{d:?}");
        }
    }

    #[test]
    fn a_chat_request_with_no_limit_gets_the_field_its_upstream_reads() {
        // OpenAI 官方的推理模型带着 `max_tokens` 整个请求 400；兼容实现大多只认 `max_tokens`
        assert_eq!(
            capped(Dialect::Chat, json!({"model": "o3"}), 100, true),
            (true, json!({"model": "o3", "max_completion_tokens": 100}))
        );
        assert_eq!(
            capped(Dialect::Chat, json!({"model": "o3"}), 100, false),
            (true, json!({"model": "o3", "max_tokens": 100}))
        );
        // 写成 `null` 的等于没写
        assert_eq!(
            capped(Dialect::Chat, json!({"max_tokens": null}), 100, true),
            (
                true,
                json!({"max_tokens": null, "max_completion_tokens": 100})
            )
        );
        assert_eq!(
            capped(Dialect::Chat, json!({"max_tokens": null}), 100, false),
            (true, json!({"max_tokens": 100}))
        );
        // 别的格式只有一个名字，和发往哪儿无关
        for official in [false, true] {
            for (d, want) in [
                (Dialect::Anthropic, json!({"max_tokens": 100})),
                (Dialect::Responses, json!({"max_output_tokens": 100})),
                (
                    Dialect::Gemini,
                    json!({"generationConfig": {"maxOutputTokens": 100}}),
                ),
                (
                    Dialect::Bedrock,
                    json!({"inferenceConfig": {"maxTokens": 100}}),
                ),
            ] {
                assert_eq!(capped(d, json!({}), 100, official), (true, want), "{d:?}");
            }
        }
    }

    #[test]
    fn thinking_still_fits_under_a_lowered_anthropic_limit() {
        let thinking = |budget: u64| json!({"type": "enabled", "budget_tokens": budget});
        // 预算不比新的上限小：压到上限减一
        assert_eq!(
            capped(
                Dialect::Anthropic,
                json!({"max_tokens": 32000, "thinking": thinking(16000)}),
                8000,
                true
            ),
            (
                true,
                json!({"max_tokens": 8000, "thinking": thinking(7999)})
            )
        );
        // 没写 max_tokens 的，填上上限也一样
        assert_eq!(
            capped(
                Dialect::Anthropic,
                json!({"thinking": thinking(8000)}),
                8000,
                false
            ),
            (
                true,
                json!({"max_tokens": 8000, "thinking": thinking(7999)})
            )
        );
        // 上限只比最小预算多一：预算正好是最小值
        assert_eq!(
            capped(
                Dialect::Anthropic,
                json!({"max_tokens": 4096, "thinking": thinking(2048)}),
                1025,
                false
            ),
            (
                true,
                json!({"max_tokens": 1025, "thinking": thinking(1024)})
            )
        );
        // 上限不比最小预算大：开不了思考，去掉它，请求照样能答
        for cap in [1024, 600] {
            assert_eq!(
                capped(
                    Dialect::Anthropic,
                    json!({"max_tokens": 4096, "thinking": thinking(2048), "x": 1}),
                    cap,
                    false
                ),
                (true, json!({"max_tokens": cap, "x": 1})),
                "{cap}"
            );
        }
        // 预算本来就小于上限的、没开思考的、自适应的：不动思考
        for t in [
            thinking(2000),
            json!({"type": "disabled"}),
            json!({"type": "adaptive"}),
        ] {
            assert_eq!(
                capped(
                    Dialect::Anthropic,
                    json!({"max_tokens": 32000, "thinking": t.clone()}),
                    8000,
                    false
                ),
                (true, json!({"max_tokens": 8000, "thinking": t}))
            );
        }
        // 客户端自己写的 max_tokens 没超上限：不是我们改出来的，不碰
        let own = json!({"max_tokens": 2000, "thinking": thinking(4000)});
        assert_eq!(
            capped(Dialect::Anthropic, own.clone(), 8000, false),
            (false, own)
        );
        // Converse 背后的 Claude 同理
        assert_eq!(
            capped(
                Dialect::Bedrock,
                json!({
                    "inferenceConfig": {"maxTokens": 32000},
                    "additionalModelRequestFields": {"thinking": thinking(16000)}
                }),
                8000,
                false
            ),
            (
                true,
                json!({
                    "inferenceConfig": {"maxTokens": 8000},
                    "additionalModelRequestFields": {"thinking": thinking(7999)}
                })
            )
        );
    }

    #[test]
    fn other_formats_keep_their_reasoning_settings_when_capped() {
        // 思考的预算只有 Anthropic（和 Converse 上的 Claude）要小于输出上限；别的格式的
        // 推理开关不归这个函数管
        for (d, v, want) in [
            (
                Dialect::Chat,
                json!({"max_tokens": 32000, "reasoning_effort": "high"}),
                json!({"max_tokens": 8000, "reasoning_effort": "high"}),
            ),
            (
                Dialect::Responses,
                json!({"max_output_tokens": 32000, "reasoning": {"effort": "high"}}),
                json!({"max_output_tokens": 8000, "reasoning": {"effort": "high"}}),
            ),
            (
                Dialect::Gemini,
                json!({"generationConfig": {
                    "maxOutputTokens": 32000,
                    "thinkingConfig": {"thinkingBudget": 16000}
                }}),
                json!({"generationConfig": {
                    "maxOutputTokens": 8000,
                    "thinkingConfig": {"thinkingBudget": 16000}
                }}),
            ),
        ] {
            assert_eq!(capped(d, v, 8000, true), (true, want), "{d:?}");
        }
    }

    #[test]
    fn the_routing_rule_setter_is_not_the_cap() {
        // 桌面版路由规则的 `set` 照旧：两个名字写了哪个改哪个（往大改也改），都没写写
        // `max_tokens`，不碰思考
        let mut v = json!({"max_completion_tokens": 50, "max_tokens": 5000});
        set_max_output_tokens(Dialect::Chat, &mut v, 9000);
        assert_eq!(
            v,
            json!({"max_completion_tokens": 9000, "max_tokens": 9000})
        );
        let mut v =
            json!({"max_tokens": 32000, "thinking": {"type": "enabled", "budget_tokens": 16000}});
        set_max_output_tokens(Dialect::Anthropic, &mut v, 8000);
        assert_eq!(
            v,
            json!({"max_tokens": 8000, "thinking": {"type": "enabled", "budget_tokens": 16000}})
        );
    }

    #[test]
    fn the_model_lives_in_the_body_except_where_it_lives_in_the_path() {
        for d in ALL {
            let mut v = json!({"model": "big"});
            set_model(d, &mut v, "small");
            let want = if matches!(d, Dialect::Gemini | Dialect::Bedrock) {
                "big"
            } else {
                "small"
            };
            assert_eq!(v["model"], want, "{d:?}");
        }
    }

    #[test]
    fn turning_thinking_off_removes_the_switch_and_nothing_else() {
        let cases = [
            (
                Dialect::Anthropic,
                json!({"thinking": {"type": "enabled"}, "x": 1}),
            ),
            (Dialect::Chat, json!({"reasoning_effort": "high", "x": 1})),
            (
                Dialect::Responses,
                json!({"reasoning": {"effort": "high"}, "x": 1}),
            ),
            (
                Dialect::Gemini,
                json!({"generationConfig": {"thinkingConfig": {}}, "x": 1}),
            ),
            (
                Dialect::Bedrock,
                json!({"additionalModelRequestFields": {"thinking": {}}, "x": 1}),
            ),
        ];
        for (d, mut v) in cases {
            disable_thinking(d, &mut v);
            let text = v.to_string();
            assert!(
                !text.contains("thinking") && !text.contains("reasoning"),
                "{d:?}: {text}"
            );
            assert_eq!(v["x"], 1);
        }
    }

    #[test]
    fn turning_thinking_off_takes_the_effort_with_it() {
        // 自适应的 Claude 只去掉开关的话，照样按 `output_config.effort` 那一档想
        let mut v = json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "xhigh"}});
        disable_thinking(Dialect::Anthropic, &mut v);
        assert_eq!(v, json!({}));
        let mut v = json!({"thinking": {"type": "adaptive"},
                           "output_config": {"effort": "high", "format": {"type": "json_schema"}}});
        disable_thinking(Dialect::Anthropic, &mut v);
        assert_eq!(
            v,
            json!({"output_config": {"format": {"type": "json_schema"}}})
        );
        let mut v = json!({"additionalModelRequestFields": {
            "thinking": {"type": "adaptive"}, "output_config": {"effort": "max"}, "top_k": 3}});
        disable_thinking(Dialect::Bedrock, &mut v);
        assert_eq!(v, json!({"additionalModelRequestFields": {"top_k": 3}}));
    }

    #[test]
    fn reasoning_is_on_where_the_decoder_says_it_is() {
        let on = [
            (
                Dialect::Anthropic,
                json!({"thinking": {"type": "enabled", "budget_tokens": 2048}}),
            ),
            (
                Dialect::Anthropic,
                json!({"thinking": {"type": "adaptive"}}),
            ),
            (
                Dialect::Anthropic,
                json!({"output_config": {"effort": "low"}}),
            ),
            (Dialect::Chat, json!({"reasoning_effort": "high"})),
            (Dialect::Chat, json!({"thinking": {"type": "enabled"}})),
            (
                Dialect::Responses,
                json!({"reasoning": {"summary": "auto"}}),
            ),
            (
                Dialect::Responses,
                json!({"reasoning": {"effort": "none"},
                "input": [{"type": "configuration_update", "reasoning": {"effort": "high"}}]}),
            ),
            (
                Dialect::Gemini,
                json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": -1}}}),
            ),
            (
                Dialect::Gemini,
                json!({"generation_config": {"thinking_config": {"thinking_level": "low"}}}),
            ),
            (
                Dialect::Bedrock,
                json!({"additionalModelRequestFields": {"thinking": {"type": "adaptive"}}}),
            ),
        ];
        for (d, v) in on {
            assert!(reasoning_on(d, &v), "{d:?}: {v}");
        }
        let off = [
            (Dialect::Anthropic, json!({})),
            (
                Dialect::Anthropic,
                json!({"thinking": {"type": "disabled"}, "output_config": {"effort": "low"}}),
            ),
            (Dialect::Chat, json!({"reasoning_effort": "none"})),
            (
                Dialect::Chat,
                json!({"thinking": {"type": "disabled"}, "reasoning_effort": "high"}),
            ),
            (
                Dialect::Chat,
                json!({"thinking": {"type": "enabled"}, "reasoning_effort": "none"}),
            ),
            (Dialect::Responses, json!({"reasoning": {"effort": "none"}})),
            (
                Dialect::Responses,
                json!({"reasoning": {"effort": "high"},
                "input": [{"type": "configuration_update", "reasoning": {"effort": "none"}}]}),
            ),
            (
                Dialect::Gemini,
                json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": 0}}}),
            ),
            (
                Dialect::Gemini,
                json!({"generationConfig": {"temperature": 1}}),
            ),
            (
                Dialect::Bedrock,
                json!({"additionalModelRequestFields": {"top_k": 3}}),
            ),
            (Dialect::Chat, json!([1])),
        ];
        for (d, v) in off {
            assert!(!reasoning_on(d, &v), "{d:?}: {v}");
            assert_eq!(reasoning_effort(d, &v), None, "{d:?}: {v}");
        }
    }

    #[test]
    fn the_effort_is_read_from_where_each_format_writes_it() {
        let cases = [
            (
                Dialect::Anthropic,
                json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "xhigh"}}),
                Some(Effort::XHigh),
            ),
            (
                Dialect::Anthropic,
                json!({"thinking": {"type": "adaptive"}}),
                None,
            ),
            (
                Dialect::Anthropic,
                json!({"thinking": {"type": "enabled", "budget_tokens": 10000}}),
                Some(Effort::Medium),
            ),
            (
                Dialect::Chat,
                json!({"reasoning_effort": "minimal"}),
                Some(Effort::Minimal),
            ),
            (
                Dialect::Chat,
                json!({"thinking": {"type": "enabled"}}),
                None,
            ),
            (
                Dialect::Responses,
                json!({"reasoning": {"effort": "medium"},
                "input": [{"type": "configuration_update", "reasoning": {"effort": "high"}}]}),
                Some(Effort::High),
            ),
            (
                Dialect::Responses,
                json!({"reasoning": {"effort": "max"}}),
                Some(Effort::Max),
            ),
            (
                Dialect::Gemini,
                json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "HIGH"}}}),
                Some(Effort::High),
            ),
            (
                Dialect::Gemini,
                json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": 4000}}}),
                Some(Effort::Low),
            ),
            (
                Dialect::Gemini,
                json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": -1}}}),
                None,
            ),
            (
                Dialect::Bedrock,
                json!({"additionalModelRequestFields": {
                "thinking": {"type": "adaptive"}, "output_config": {"effort": "low"}}}),
                Some(Effort::Low),
            ),
        ];
        for (d, v, want) in cases {
            assert_eq!(reasoning_effort(d, &v), want, "{d:?}: {v}");
        }
    }

    #[test]
    fn setting_the_effort_writes_the_field_the_client_wrote() {
        let set = |d, mut v: Value, e| {
            let changed = set_reasoning_effort(d, &mut v, e);
            (changed, v)
        };
        assert_eq!(
            set(
                Dialect::Anthropic,
                json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "xhigh"}}),
                Effort::Max
            ),
            (
                true,
                json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "max"}})
            )
        );
        // 自适应、没写强度：补上
        assert_eq!(
            set(
                Dialect::Anthropic,
                json!({"thinking": {"type": "adaptive"}}),
                Effort::High
            ),
            (
                true,
                json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "high"}})
            )
        );
        // 按预算的改预算，要小于最大输出
        assert_eq!(
            set(
                Dialect::Anthropic,
                json!({"max_tokens": 20000, "thinking": {"type": "enabled", "budget_tokens": 4000}}),
                Effort::Max
            ),
            (
                true,
                json!({"max_tokens": 20000, "thinking": {"type": "enabled", "budget_tokens": 19999}})
            )
        );
        let tiny =
            json!({"max_tokens": 1000, "thinking": {"type": "enabled", "budget_tokens": 1024}});
        assert_eq!(
            set(Dialect::Anthropic, tiny.clone(), Effort::Max),
            (false, tiny)
        );
        // 已经是这一档：没改
        let same = json!({"reasoning_effort": "high"});
        assert_eq!(
            set(Dialect::Chat, same.clone(), Effort::High),
            (false, same)
        );
        assert_eq!(
            set(
                Dialect::Chat,
                json!({"thinking": {"type": "enabled"}}),
                Effort::Low
            ),
            (
                true,
                json!({"thinking": {"type": "enabled"}, "reasoning_effort": "low"})
            )
        );
        assert_eq!(
            set(
                Dialect::Responses,
                json!({"reasoning": {"effort": "xhigh", "summary": "auto"},
                "input": [{"type": "configuration_update", "reasoning": {"effort": "medium"}}, {"type": "message"}]}),
                Effort::Max
            ),
            (
                true,
                json!({"reasoning": {"effort": "max", "summary": "auto"},
                "input": [{"type": "configuration_update", "reasoning": {"effort": "max"}}, {"type": "message"}]})
            )
        );
        assert_eq!(
            set(
                Dialect::Gemini,
                json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "low"}}}),
                Effort::Max
            ),
            (
                true,
                json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "HIGH"}}})
            )
        );
        assert_eq!(
            set(
                Dialect::Gemini,
                json!({"generation_config": {"thinking_config": {"thinking_budget": 1024}}}),
                Effort::High
            ),
            (
                true,
                json!({"generation_config": {"thinking_config": {"thinking_budget": 32000}}})
            )
        );
        // Gemini 让模型自己定的：不知道它认等级还是预算，不改
        let dynamic = json!({"generationConfig": {"thinkingConfig": {"includeThoughts": true}}});
        assert_eq!(
            set(Dialect::Gemini, dynamic.clone(), Effort::High),
            (false, dynamic)
        );
        assert_eq!(
            set(
                Dialect::Bedrock,
                json!({"inferenceConfig": {"maxTokens": 9000},
                "additionalModelRequestFields": {"thinking": {"type": "enabled", "budget_tokens": 2000}}}),
                Effort::High
            ),
            (
                true,
                json!({"inferenceConfig": {"maxTokens": 9000},
                "additionalModelRequestFields": {"thinking": {"type": "enabled", "budget_tokens": 8999}}})
            )
        );
        assert_eq!(
            set(
                Dialect::Bedrock,
                json!({"additionalModelRequestFields": {
                "thinking": {"type": "adaptive"}, "output_config": {"effort": "xhigh"}}}),
                Effort::High
            ),
            (
                true,
                json!({"additionalModelRequestFields": {
                "thinking": {"type": "adaptive"}, "output_config": {"effort": "high"}}})
            )
        );
    }

    #[test]
    fn a_request_without_reasoning_is_never_given_any() {
        for (d, v) in [
            (Dialect::Anthropic, json!({"max_tokens": 10})),
            (
                Dialect::Anthropic,
                json!({"thinking": {"type": "disabled"}}),
            ),
            (Dialect::Chat, json!({"reasoning_effort": "none"})),
            (Dialect::Responses, json!({"input": "hi"})),
            (
                Dialect::Gemini,
                json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": 0}}}),
            ),
            (
                Dialect::Bedrock,
                json!({"inferenceConfig": {"maxTokens": 10}}),
            ),
        ] {
            let mut w = v.clone();
            assert!(!set_reasoning_effort(d, &mut w, Effort::Max), "{d:?}");
            assert!(!remove_reasoning_effort(d, &mut w), "{d:?}");
            assert_eq!(w, v, "{d:?}");
        }
    }

    #[test]
    fn removing_the_effort_leaves_reasoning_on() {
        let remove = |d, mut v: Value| {
            let changed = remove_reasoning_effort(d, &mut v);
            (changed, v)
        };
        assert_eq!(
            remove(
                Dialect::Anthropic,
                json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "xhigh"}})
            ),
            (true, json!({"thinking": {"type": "adaptive"}}))
        );
        // 预算是开思考必须的：不动
        let budget = json!({"thinking": {"type": "enabled", "budget_tokens": 4000}});
        assert_eq!(remove(Dialect::Anthropic, budget.clone()), (false, budget));
        assert_eq!(
            remove(
                Dialect::Chat,
                json!({"thinking": {"type": "enabled"}, "reasoning_effort": "max"})
            ),
            (true, json!({"thinking": {"type": "enabled"}}))
        );
        assert_eq!(
            remove(
                Dialect::Responses,
                json!({"reasoning": {"effort": "xhigh", "summary": "auto"},
                "input": [{"type": "configuration_update", "reasoning": {"effort": "high"}}, {"type": "message"}]})
            ),
            (
                true,
                json!({"reasoning": {"summary": "auto"}, "input": [{"type": "message"}]})
            )
        );
        assert_eq!(
            remove(
                Dialect::Gemini,
                json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "LOW", "includeThoughts": true}}})
            ),
            (
                true,
                json!({"generationConfig": {"thinkingConfig": {"includeThoughts": true}}})
            )
        );
        assert_eq!(
            remove(
                Dialect::Bedrock,
                json!({"additionalModelRequestFields": {
                "thinking": {"type": "adaptive"}, "output_config": {"effort": "max"}}})
            ),
            (
                true,
                json!({"additionalModelRequestFields": {"thinking": {"type": "adaptive"}}})
            )
        );
    }

    #[test]
    fn what_is_set_is_what_is_read_back() {
        let bodies = [
            (
                Dialect::Anthropic,
                json!({"thinking": {"type": "adaptive"}}),
            ),
            (Dialect::Chat, json!({"reasoning_effort": "low"})),
            (Dialect::Responses, json!({"reasoning": {}})),
            (
                Dialect::Bedrock,
                json!({"additionalModelRequestFields": {"thinking": {"type": "adaptive"}}}),
            ),
        ];
        for (d, v) in bodies {
            for e in [
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::XHigh,
                Effort::Max,
            ] {
                let mut w = v.clone();
                set_reasoning_effort(d, &mut w, e);
                assert_eq!(reasoning_effort(d, &w), Some(e), "{d:?}: {w}");
            }
        }
    }
}
