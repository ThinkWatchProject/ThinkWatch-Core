//! 嵌入和旧版补全的请求视图：**一项输入一条消息**。
//!
//! - OpenAI 的嵌入（`/v1/embeddings`）的 `input`、旧版补全（`/v1/completions`）的
//!   `prompt`：一个字符串是一条消息；数组里每一项一条 —— 字符串是一段文字，一串 token
//!   （数字数组）是一个只读的 `other` 部分（`label` 是 `tokens`）。整个就是一串 token
//!   （数组里全是数字）的，是一条消息。
//! - Gemini 的嵌入：`:embedContent` 的 `content`、`:batchEmbedContents` 里每个请求的
//!   `content`，一个 Content 一条消息，它的每个部分一个部分：文字是文字，别的只读。
//!
//! 消息都是 `user`。没有 `system`、没有 `tools`。`params`：嵌入只有 `model`；补全是
//! `model`、`max_tokens`、`temperature`、`top_p`、`stop`。`suffix`、`dimensions`、
//! `taskType` 这些不给看、不动。
//!
//! # 改写规则比对话严
//!
//! **只有文字能改**：消息和部分不能加、不能删、不能挪 —— 上游按输入的先后一项一项地回
//! （第几个向量、第几段补全），多一项少一项，客户端拿到的回答就对不上号了。核对在
//! [`check`]，写回（[`apply`]）再守一道。
//!
//! 写回只碰改过的那几段文字（和改了的参数）：别的字段原样留着。

use serde_json::{Map, Value, json};

use super::*;

/// 视图里每条消息在原文里是哪一项，和它有几个部分。
pub struct Src {
    form: Form,
    items: Vec<(Item, usize)>,
    /// 补全的 `stop` 原来是一个字符串
    stop_string: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    /// OpenAI：`input` / `prompt` 整个（一个字符串、一串 token、认不出的值）
    Whole,
    /// OpenAI：数组里的第几项
    At(usize),
    /// Gemini：`:embedContent` 的 `content`
    Content,
    /// Gemini：`:batchEmbedContents` 里第几个请求的 `content`
    Request(usize),
}

impl Form {
    /// OpenAI 那两种的输入写在哪个字段里
    fn field(self) -> &'static str {
        match self {
            Form::OpenaiCompletions => "prompt",
            _ => "input",
        }
    }

    /// 视图的 `params` 里有的那几个
    fn params(self) -> &'static [&'static str] {
        match self {
            Form::OpenaiCompletions => &["model", "max_tokens", "temperature", "top_p", "stop"],
            _ => &["model"],
        }
    }

    /// 报错时怎么称呼这种请求
    fn noun(self) -> &'static str {
        match self {
            Form::OpenaiCompletions => "a completions request",
            _ => "an embeddings request",
        }
    }
}

/// 一项输入（OpenAI 那两种）在视图里的那个部分
fn openai_part(key: &str, v: &Value) -> Value {
    match v {
        Value::String(s) => part_text(key, s),
        other => part_other(key, label(other)),
    }
}

/// 读不成文字的一项叫什么。一串 token 是 `tokens`
fn label(v: &Value) -> &str {
    match v {
        Value::Array(a) if a.iter().all(Value::is_number) => "tokens",
        Value::Array(_) => "array",
        Value::Object(o) => o.get("type").and_then(Value::as_str).unwrap_or("object"),
        Value::Number(_) => "number",
        Value::Bool(_) => "boolean",
        Value::Null => "null",
        Value::String(_) => "text",
    }
}

/// 一串 token：不空、全是数字的数组
fn tokens(a: &[Value]) -> bool {
    !a.is_empty() && a.iter().all(Value::is_number)
}

pub fn build(form: Form, raw: &Value, path: &str) -> Result<Built, String> {
    let mut view = Map::new();
    view.insert("format".into(), json!(form.name()));
    let mut messages = Vec::new();
    let mut items = Vec::new();
    let mut push = |item: Item, parts: Vec<Value>| {
        let i = messages.len();
        items.push((item, parts.len()));
        messages.push(json!({ "key": msg_key(i), "role": "user", "parts": parts }));
    };
    let mut params = Map::new();
    let mut stop_string = false;
    match form {
        Form::OpenaiEmbeddings | Form::OpenaiCompletions => {
            let model = raw.get("model").and_then(Value::as_str).unwrap_or_default();
            view.insert("model".into(), json!(model));
            params.insert("model".into(), json!(model));
            let key = |i: usize| part_key(i, 0);
            match raw.get(form.field()) {
                None | Some(Value::Null) => {}
                // 一串 token 是一项输入，不是每个数字一项
                Some(Value::Array(a)) if !tokens(a) => {
                    for (i, x) in a.iter().enumerate() {
                        push(Item::At(i), vec![openai_part(&key(i), x)]);
                    }
                }
                Some(whole) => push(Item::Whole, vec![openai_part(&key(0), whole)]),
            }
            if form == Form::OpenaiCompletions {
                if let Some(n) = raw.get("max_tokens").and_then(Value::as_u64) {
                    params.insert("max_tokens".into(), json!(n));
                }
                for k in ["temperature", "top_p"] {
                    if let Some(x) = raw.get(k).filter(|x| x.is_number()) {
                        params.insert(k.into(), x.clone());
                    }
                }
                match raw.get("stop") {
                    Some(Value::String(s)) => {
                        stop_string = true;
                        params.insert("stop".into(), json!([s]));
                    }
                    Some(Value::Array(a)) => {
                        params.insert(
                            "stop".into(),
                            Value::Array(a.iter().filter(|s| s.is_string()).cloned().collect()),
                        );
                    }
                    _ => {}
                }
            }
        }
        Form::GeminiEmbed => {
            let model = gemini::path_model(path).ok_or_else(|| {
                format!("the path {path} does not say which Gemini model to call")
            })?;
            view.insert("model".into(), json!(model));
            params.insert("model".into(), json!(model));
            if batch(path) {
                for (i, r) in raw
                    .get("requests")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    push(Item::Request(i), content_parts(i, r.get("content")));
                }
            } else if let Some(c) = raw.get("content") {
                push(Item::Content, content_parts(0, Some(c)));
            }
        }
        Form::Conversation(_) => return Err("a conversation is not a list of inputs".into()),
    }
    view.insert("messages".into(), Value::Array(messages));
    view.insert("params".into(), Value::Object(params));
    Ok(Built {
        view: Value::Object(view),
        src: super::Src::Inputs(Src {
            form,
            items,
            stop_string,
        }),
    })
}

/// 每项输入里读得出的文字，按先后：内容过滤查的那一份。
///
/// 嵌入、补全开头不过内容过滤（只有输入，分不出调用方自己打的字和工具抓回来的，见
/// [`crate::client_api::ClientApi::screened`]），**插件写进去的字照样要查**：插件改过之后，
/// 管线拿改前、改后的文字各查一遍，只报插件加进来的（见 `server::pipeline::plug`）。读不成
/// 视图的是空的
pub fn texts(form: Form, raw: &Value, path: &str) -> Vec<String> {
    let view = build(form, raw, path)
        .map(|b| b.view)
        .unwrap_or(Value::Null);
    view["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["parts"].as_array().into_iter().flatten())
        .filter(|p| p["type"] == "text")
        .filter_map(|p| p["text"].as_str())
        .map(str::to_string)
        .collect()
}

/// 按 [`texts`] 的先后逐段改每项输入的文字，改在原文那一项上：内容过滤删过之后写回
/// （见 `server::pipeline::plug`）。别的字段不动；读不成视图的什么都不做
pub fn rewrite_texts(form: Form, raw: &mut Value, path: &str, mut f: impl FnMut(&mut String)) {
    let Ok(built) = build(form, raw, path) else {
        return;
    };
    let super::Src::Inputs(src) = &built.src else {
        return;
    };
    let Some(obj) = raw.as_object_mut() else {
        return;
    };
    for (k, m) in built.view["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        for (j, p) in m["parts"].as_array().into_iter().flatten().enumerate() {
            if p["type"] != "text" {
                continue;
            }
            if let Some(Value::String(t)) = slot(obj, src, src.items[k].0, j) {
                f(t);
            }
        }
    }
}

/// `:batchEmbedContents`（一个请求里好几项输入）
fn batch(path: &str) -> bool {
    path.trim_end_matches('/').ends_with(":batchEmbedContents")
}

/// Gemini 的一个 Content 的部分：有文字的是文字，别的只读
fn content_parts(i: usize, content: Option<&Value>) -> Vec<Value> {
    content
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(j, p)| {
            let key = part_key(i, j);
            match gemini::fstr(p, "text") {
                Some(t) => part_text(&key, t),
                None => part_other(
                    &key,
                    p.as_object()
                        .and_then(|o| o.keys().next())
                        .map_or("unknown", String::as_str),
                ),
            }
        })
        .collect()
}

/// 核对：先按对话的规矩（key、权限、只读的部分），再加上这一种的：**消息和部分一个都
/// 不能多、不能少**，参数只能改这种请求有的那几个。
pub fn check(
    src: &Src,
    input: &Value,
    output: &Value,
    perms: &[Permission],
) -> Result<Edits, EditError> {
    let edits = super::check(input, output, perms, &[])?;
    if let Some(ms) = &edits.messages {
        fixed(src, ms)?;
    }
    if let Some(p) = &edits.params {
        params_allowed(src.form, p)?;
    }
    Ok(edits)
}

/// 消息、部分都是原来那些，一一对应、先后不变
fn fixed(src: &Src, ms: &[MsgEdit]) -> Result<(), EditError> {
    let noun = src.form.noun();
    let added = || {
        bad(format!(
            "messages cannot be added to {noun}: each message is one input, and the answer \
             comes back input by input"
        ))
    };
    for (k, e) in ms.iter().enumerate() {
        let (from, parts) = match e {
            MsgEdit::Insert { .. } => return Err(added()),
            MsgEdit::Keep { from, parts } => (*from, parts),
        };
        if from != k {
            return Err(removed(noun));
        }
        let Some(&(_, count)) = src.items.get(from) else {
            return Err(added());
        };
        let Some(parts) = parts else { continue };
        let kept = parts.len() == count
            && parts
                .iter()
                .enumerate()
                .all(|(j, p)| matches!(p, PartEdit::Keep { from, .. } if *from == j));
        if !kept {
            return Err(bad(format!(
                "parts cannot be added to or removed from the messages of {noun}; only the text \
                 of a text part can change"
            )));
        }
    }
    if ms.len() != src.items.len() {
        return Err(removed(noun));
    }
    Ok(())
}

fn removed(noun: &str) -> EditError {
    bad(format!(
        "messages cannot be removed from {noun}: each message is one input, and the answer \
         comes back input by input"
    ))
}

/// 参数只改这种请求有的那几个：嵌入只有 `model`
fn params_allowed(form: Form, p: &ParamsEdit) -> Result<(), EditError> {
    let changed = [
        ("max_tokens", p.max_tokens.is_some()),
        ("temperature", p.temperature.is_some()),
        ("top_p", p.top_p.is_some()),
        ("stop", p.stop.is_some()),
    ];
    match changed
        .iter()
        .find(|(k, c)| *c && !form.params().contains(k))
    {
        Some((k, _)) => Err(bad(format!(
            "{} has no `params.{k}`; only {} can change",
            form.noun(),
            form.params()
                .iter()
                .map(|k| format!("`params.{k}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
        None => Ok(()),
    }
}

/// 写回：改过的文字落回原来那一项，参数照这种请求的写法。返回新的路径（Gemini 换了
/// 模型时）。
pub fn apply(
    raw: &mut Value,
    src: &Src,
    edits: &Edits,
    path: &str,
) -> Result<Option<String>, EditError> {
    let Some(obj) = raw.as_object_mut() else {
        return Err(bad("the request body is not a JSON object"));
    };
    if edits.system.is_some() || edits.tools.is_some() {
        return Err(bad(format!(
            "{} has no system prompt and no tools",
            src.form.noun()
        )));
    }
    if let Some(ms) = &edits.messages {
        fixed(src, ms)?;
        for (k, e) in ms.iter().enumerate() {
            let MsgEdit::Keep {
                parts: Some(parts), ..
            } = e
            else {
                continue;
            };
            for (j, p) in parts.iter().enumerate() {
                if let PartEdit::Keep {
                    change: Some(change),
                    ..
                } = p
                {
                    let Change::Text(t) = change else {
                        return Err(bad("only the text of an input can change"));
                    };
                    set_text(obj, src, src.items[k].0, j, t)?;
                }
            }
        }
    }
    let Some(p) = &edits.params else {
        return Ok(None);
    };
    params_allowed(src.form, p)?;
    match src.form {
        Form::GeminiEmbed => {
            let Some(m) = &p.model else {
                return Ok(None);
            };
            // 路径上是模型，请求体里的 `model`（`models/…`）也写着它：两处对得上上游才收
            let named = json!(format!("models/{m}"));
            if batch(path) {
                for r in obj
                    .get_mut("requests")
                    .and_then(Value::as_array_mut)
                    .into_iter()
                    .flatten()
                {
                    if let Some(slot) = r.get_mut("model").filter(|x| x.is_string()) {
                        *slot = named.clone();
                    }
                }
            } else if let Some(slot) = obj.get_mut("model").filter(|x| x.is_string()) {
                *slot = named;
            }
            Ok(Some(crate::forward::gemini_path_with_model(path, m)))
        }
        _ => {
            if let Some(m) = &p.model {
                obj.insert("model".into(), json!(m));
            }
            anthropic::set_opt(obj, "max_tokens", p.max_tokens.map(|o| o.map(Value::from)));
            anthropic::set_opt(
                obj,
                "temperature",
                p.temperature.map(|o| o.map(Value::from)),
            );
            anthropic::set_opt(obj, "top_p", p.top_p.map(|o| o.map(Value::from)));
            anthropic::set_opt(
                obj,
                "stop",
                p.stop.clone().map(|o| {
                    o.map(|s| match s.as_slice() {
                        [one] if src.stop_string => json!(one),
                        _ => json!(s),
                    })
                }),
            );
            Ok(None)
        }
    }
}

/// 第 `part` 个部分的文字换成 `t`。**只换得了原来就是文字的那一项**
fn set_text(
    obj: &mut Map<String, Value>,
    src: &Src,
    item: Item,
    part: usize,
    t: &str,
) -> Result<(), EditError> {
    match slot(obj, src, item, part) {
        Some(v @ Value::String(_)) => {
            *v = json!(t);
            Ok(())
        }
        _ => Err(bad("only the text of an input can change")),
    }
}

/// 一项输入的第 `part` 个部分在原文里的那个值（文字的话是那个字符串）
fn slot<'v>(
    obj: &'v mut Map<String, Value>,
    src: &Src,
    item: Item,
    part: usize,
) -> Option<&'v mut Value> {
    match item {
        Item::Whole => obj.get_mut(src.form.field()),
        Item::At(i) => obj.get_mut(src.form.field()).and_then(|v| v.get_mut(i)),
        Item::Content => obj
            .get_mut("content")
            .and_then(|c| c.get_mut("parts"))
            .and_then(|p| p.get_mut(part))
            .and_then(|p| p.get_mut("text")),
        Item::Request(i) => obj
            .get_mut("requests")
            .and_then(|r| r.get_mut(i))
            .and_then(|r| r.get_mut("content"))
            .and_then(|c| c.get_mut("parts"))
            .and_then(|p| p.get_mut(part))
            .and_then(|p| p.get_mut("text")),
    }
}
