//! 请求视图：插件看到的那一份请求，和它交回来之后怎么写回原来的 JSON。
//!
//! # 为什么不用中间表示
//!
//! 中间表示是给转换用的：同一角色的相邻消息会并成一条，只有一种格式有的东西解码时就
//! 丢了。插件改完的请求要**写回客户端发来的那一份**（同格式直通时原样发给上游），
//! 所以视图直接从客户端的 JSON 读，每一项记着自己在原文里的位置：消息是数组里的
//! 第几个，部分是哪个字段、内容数组里的第几块。
//!
//! # 一项一个 `key`
//!
//! 每条消息、每个部分、每个工具带一个网关发的 `key`。插件留着 key 就是改它，删掉
//! 这一项就是删，没有 key 的是新加的。写回时只碰改过的那几项：缓存断点
//! （`cache_control`）、推理签名、图片、不认识的字段都留在原来的对象上，原样留着。
//!
//! # 核对（[`check`]）
//!
//! 插件交回来的东西先对着它拿到的那一份核一遍：没给的部分不许出现（权限）、key 认不
//! 认识、有没有重复、留下来的有没有挪位置、只读的东西改没改。核对只看视图本身，
//! 和格式无关；写回时格式自己的限制（Anthropic 的消息里没有 system 角色）由各格式
//! 报。

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};
use tw_api::Permission;
use tw_dialect::ir::Dialect;
use tw_types::{Msg, msg};

use super::bridge::Bridge;

pub mod anthropic;
pub mod chat;
pub mod gemini;
pub mod responses;
mod segments;

pub(crate) use segments::{Seg, diff as segment_diff};

/// 插件交回来的东西不合规矩。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// 动了没给它的部分，或者改了只读的东西
    PermissionViolation(String),
    /// 形状不对：不认识的 key、重复的 key、挪了位置、类型不对
    BadOutput(String),
}

impl EditError {
    /// 记在这次运行上、报给客户端的那一句
    pub fn msg(&self) -> Msg {
        match self {
            EditError::PermissionViolation(detail) => msg!(
                "gw.plugin.permission_violation", detail = detail.clone() =>
                "The plugin changed something it has no permission to change: {detail}"
            ),
            // 和运行时查出来的形状不对是同一句
            EditError::BadOutput(detail) => {
                crate::plugin::host::RunError::BadOutput(detail.clone()).msg()
            }
        }
    }
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::PermissionViolation(why) => write!(f, "permission violation: {why}"),
            EditError::BadOutput(why) => write!(f, "bad output: {why}"),
        }
    }
}

fn bad(why: impl Into<String>) -> EditError {
    EditError::BadOutput(why.into())
}

fn denied(why: impl Into<String>) -> EditError {
    EditError::PermissionViolation(why.into())
}

/// 消息在视图里的角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    /// 只装工具结果的消息
    Tool,
    /// 对话中途的 system / developer 消息
    System,
}

impl Role {
    pub fn slug(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
            Role::System => "system",
        }
    }

    fn parse(s: &str) -> Option<Role> {
        Some(match s {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "tool" => Role::Tool,
            "system" => Role::System,
            _ => return None,
        })
    }
}

/// 一份读好的请求：完整的视图（还没按权限裁），和写回时要用的位置。
pub struct Built {
    pub view: Value,
    pub src: Src,
}

/// 视图里每一项在原文里的位置，按格式各记各的。
pub enum Src {
    Anthropic(anthropic::Src),
    Chat(chat::Src),
    Responses(responses::Src),
    Gemini(gemini::Src),
}

impl Src {
    /// 视图里看不到的工具的名字（服务端工具、托管工具）。**新加的工具不许和它们重名**
    pub fn hidden_tools(&self) -> &[String] {
        match self {
            Src::Anthropic(s) => &s.hidden_tools,
            Src::Chat(s) => &s.hidden_tools,
            Src::Responses(s) => &s.hidden_tools,
            Src::Gemini(s) => &s.hidden_tools,
        }
    }
}

/// 把客户端发来的请求读成视图。`path` 是客户端请求的路径（Gemini 的模型写在里面）。
///
/// 不是 JSON 对象、或者不是这四种格式的，读不出来。
pub fn build(dialect: Dialect, raw: &Value, path: &str) -> Result<Built, String> {
    if !raw.is_object() {
        return Err("the request body is not a JSON object".into());
    }
    match dialect {
        Dialect::Anthropic => Ok(anthropic::build(raw)),
        Dialect::Chat => Ok(chat::build(raw)),
        Dialect::Responses => Ok(responses::build(raw)),
        Dialect::Gemini => gemini::build(raw, path),
        Dialect::Bedrock => Err("Bedrock is not a client format".into()),
    }
}

/// 写回：把核对过、占位符已经换回去的改动写进原文。返回新的路径（Gemini 换了模型时）。
pub fn apply(
    raw: &mut Value,
    src: &Src,
    edits: &Edits,
    path: &str,
) -> Result<Option<String>, EditError> {
    match src {
        Src::Anthropic(s) => anthropic::apply(raw, s, edits).map(|_| None),
        Src::Chat(s) => chat::apply(raw, s, edits).map(|_| None),
        Src::Responses(s) => responses::apply(raw, s, edits).map(|_| None),
        Src::Gemini(s) => gemini::apply(raw, s, edits, path),
    }
}

/// 按权限裁掉没给的部分。`format` 和 `model` 总在。
pub fn trim(view: &Value, perms: &[Permission]) -> Value {
    let mut out = Map::new();
    for (k, v) in view.as_object().into_iter().flatten() {
        let keep = match k.as_str() {
            "format" | "model" => true,
            "system" => perms.contains(&Permission::System),
            "messages" => perms.contains(&Permission::Messages),
            "tools" => perms.contains(&Permission::Tools),
            "params" => perms.contains(&Permission::Params),
            _ => false,
        };
        if keep {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

// ───────────────────────────────────────────────────────── 改动

/// 核对过的改动。**`None` 是这一部分没动。**
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Edits {
    /// 新的系统提示；`""` 是去掉
    pub system: Option<String>,
    pub messages: Option<Vec<MsgEdit>>,
    pub tools: Option<Vec<ToolEdit>>,
    pub params: Option<ParamsEdit>,
}

impl Edits {
    pub fn is_empty(&self) -> bool {
        self.system.is_none()
            && self.messages.is_none()
            && self.tools.is_none()
            && self.params.as_ref().is_none_or(ParamsEdit::is_empty)
    }

    /// 占位符换回原值。核对是对着插件看到的那一份（带占位符）做的，写回的是真值
    pub fn reveal(&mut self, b: &Bridge) {
        if b.is_empty() {
            return;
        }
        if let Some(s) = &mut self.system {
            *s = b.reveal(s);
        }
        for m in self.messages.iter_mut().flatten() {
            match m {
                MsgEdit::Insert { texts, .. } => texts.iter_mut().for_each(|t| *t = b.reveal(t)),
                MsgEdit::Keep { parts, .. } => {
                    for p in parts.iter_mut().flatten() {
                        match p {
                            PartEdit::Insert(t) => *t = b.reveal(t),
                            PartEdit::Keep { change, .. } => match change {
                                Some(Change::Text(t)) | Some(Change::Result(t)) => *t = b.reveal(t),
                                Some(Change::Input(v)) => b.reveal_value(v),
                                None => {}
                            },
                        }
                    }
                }
            }
        }
        for t in self.tools.iter_mut().flatten() {
            match t {
                ToolEdit::Keep {
                    description,
                    schema,
                    ..
                } => {
                    if let Some(d) = description {
                        *d = b.reveal(d);
                    }
                    if let Some(s) = schema {
                        b.reveal_value(s);
                    }
                }
                ToolEdit::Insert {
                    name,
                    description,
                    schema,
                } => {
                    *name = b.reveal(name);
                    *description = b.reveal(description);
                    b.reveal_value(schema);
                }
            }
        }
        if let Some(p) = &mut self.params {
            if let Some(m) = &mut p.model {
                *m = b.reveal(m);
            }
            if let Some(Some(stop)) = &mut p.stop {
                stop.iter_mut().for_each(|s| *s = b.reveal(s));
            }
        }
    }
}

/// 一条消息的去向。顺序就是写回之后的顺序。
#[derive(Debug, Clone, PartialEq)]
pub enum MsgEdit {
    /// 留下视图里的第 `from` 条。`parts` 是 `None` 时整条原样
    Keep {
        from: usize,
        parts: Option<Vec<PartEdit>>,
    },
    /// 新加的一条，只有文字
    Insert { role: Role, texts: Vec<String> },
}

/// 一个部分的去向。
#[derive(Debug, Clone, PartialEq)]
pub enum PartEdit {
    /// 留下这条消息的第 `from` 个部分，可能改了能改的那个字段
    Keep { from: usize, change: Option<Change> },
    /// 新加的一段文字
    Insert(String),
}

/// 能改的字段改成了什么。
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// 文字部分的 `text`
    Text(String),
    /// 工具调用的 `input`
    Input(Value),
    /// 工具结果的 `text`
    Result(String),
}

/// 一个工具的去向。
#[derive(Debug, Clone, PartialEq)]
pub enum ToolEdit {
    Keep {
        from: usize,
        description: Option<String>,
        schema: Option<Value>,
    },
    Insert {
        name: String,
        description: String,
        schema: Value,
    },
}

/// 参数的改动。外层 `Some` 是改了，里层 `None` 是去掉了这个字段。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParamsEdit {
    pub model: Option<String>,
    pub max_tokens: Option<Option<u64>>,
    pub temperature: Option<Option<f64>>,
    pub top_p: Option<Option<f64>>,
    pub stop: Option<Option<Vec<String>>>,
}

impl ParamsEdit {
    pub fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.max_tokens.is_none()
            && self.temperature.is_none()
            && self.top_p.is_none()
            && self.stop.is_none()
    }
}

// ───────────────────────────────────────────────────────── 核对

/// 把插件交回来的视图对着它拿到的那一份核一遍，得出改了什么。
///
/// `input` 是插件拿到的那一份（裁过、占位符换过）；`hidden_tools` 是视图里看不到的
/// 工具名，新加的工具不许和它们重名。
pub fn check(
    input: &Value,
    output: &Value,
    perms: &[Permission],
    hidden_tools: &[String],
) -> Result<Edits, EditError> {
    let Some(out) = output.as_object() else {
        return Err(bad("the plugin returned something other than an object"));
    };
    let mut edits = Edits::default();
    for (k, v) in out {
        match k.as_str() {
            "format" | "model" => {
                if input.get(k) != Some(v) {
                    return Err(denied(if k == "model" {
                        "`model` is read-only; change `params.model` instead".to_string()
                    } else {
                        "`format` is read-only".to_string()
                    }));
                }
            }
            "system" => {
                need(perms, Permission::System, "system")?;
                let Some(s) = v.as_str() else {
                    return Err(bad("`system` must be a string"));
                };
                if input.get("system").and_then(Value::as_str) != Some(s) {
                    edits.system = Some(s.to_string());
                }
            }
            "messages" => {
                need(perms, Permission::Messages, "messages")?;
                edits.messages = check_messages(input, v)?;
            }
            "tools" => {
                need(perms, Permission::Tools, "tools")?;
                edits.tools = check_tools(input, v, hidden_tools)?;
            }
            "params" => {
                need(perms, Permission::Params, "params")?;
                let p = check_params(input, v)?;
                if !p.is_empty() {
                    edits.params = Some(p);
                }
            }
            other => return Err(bad(format!("unknown field `{other}`"))),
        }
    }
    Ok(edits)
}

fn need(perms: &[Permission], p: Permission, section: &str) -> Result<(), EditError> {
    if perms.contains(&p) {
        Ok(())
    } else {
        Err(denied(format!(
            "`{section}` was returned without the {} permission",
            p.slug()
        )))
    }
}

fn only_fields(o: &Map<String, Value>, allowed: &[&str], what: &str) -> Result<(), EditError> {
    match o.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(bad(format!("{what} has an unknown field `{k}`"))),
        None => Ok(()),
    }
}

fn check_messages(input: &Value, out: &Value) -> Result<Option<Vec<MsgEdit>>, EditError> {
    let empty = Vec::new();
    let inp = input
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let Some(out) = out.as_array() else {
        return Err(bad("`messages` must be an array"));
    };
    let keys: HashMap<&str, usize> = inp
        .iter()
        .enumerate()
        .filter_map(|(i, m)| Some((m.get("key")?.as_str()?, i)))
        .collect();
    // 每个部分的 key 属于哪条消息：拿别的消息的部分来用要说清楚
    let mut owner: HashMap<&str, usize> = HashMap::new();
    for (i, m) in inp.iter().enumerate() {
        for p in m
            .get("parts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(k) = p.get("key").and_then(Value::as_str) {
                owner.insert(k, i);
            }
        }
    }
    let mut seen = HashSet::new();
    let mut last: Option<usize> = None;
    let mut changed = false;
    let mut edits = Vec::with_capacity(out.len());
    for (n, m) in out.iter().enumerate() {
        let Some(o) = m.as_object() else {
            return Err(bad(format!("messages[{n}] is not an object")));
        };
        only_fields(o, &["key", "role", "parts"], &format!("messages[{n}]"))?;
        let Some(role) = o.get("role").and_then(Value::as_str) else {
            return Err(bad(format!("messages[{n}].role must be a string")));
        };
        let Some(parts) = o.get("parts").and_then(Value::as_array) else {
            return Err(bad(format!("messages[{n}].parts must be an array")));
        };
        match o.get("key") {
            Some(Value::String(key)) => {
                let Some(&i) = keys.get(key.as_str()) else {
                    return Err(bad(format!("messages[{n}] has an unknown key `{key}`")));
                };
                if !seen.insert(i) {
                    return Err(bad(format!("the key `{key}` appears twice")));
                }
                if last.is_some_and(|l| i < l) {
                    return Err(bad(format!(
                        "message `{key}` moved; kept messages stay in their original order"
                    )));
                }
                last = Some(i);
                let orig = &inp[i];
                if orig.get("role").and_then(Value::as_str) != Some(role) {
                    return Err(denied(format!("the role of message `{key}` is read-only")));
                }
                let parts = check_parts(orig, parts, key, &owner, i)?;
                changed |= parts.is_some();
                edits.push(MsgEdit::Keep { from: i, parts });
            }
            Some(_) => return Err(bad(format!("messages[{n}].key must be a string"))),
            None => {
                changed = true;
                let role = match Role::parse(role) {
                    Some(r @ (Role::User | Role::Assistant | Role::System)) => r,
                    _ => {
                        return Err(bad(format!(
                            "messages[{n}] is new, and a new message can only be user, assistant or system"
                        )));
                    }
                };
                let mut texts = Vec::with_capacity(parts.len());
                for p in parts {
                    match new_text(p) {
                        Some(t) => texts.push(t),
                        None => {
                            return Err(bad(format!(
                                "messages[{n}] is new, and a new message may contain only text parts \
                                 ({{\"type\": \"text\", \"text\": \"…\"}})"
                            )));
                        }
                    }
                }
                if texts.is_empty() {
                    return Err(bad(format!(
                        "messages[{n}] is new and has no parts; a new message needs some text"
                    )));
                }
                edits.push(MsgEdit::Insert { role, texts });
            }
        }
    }
    changed |= seen.len() != inp.len();
    Ok(changed.then_some(edits))
}

/// 一个新加的文字部分里的字。**只认这一种形状**：不带 key，只有 type 和 text
fn new_text(p: &Value) -> Option<String> {
    let o = p.as_object()?;
    if o.len() != 2 || o.get("type").and_then(Value::as_str) != Some("text") {
        return None;
    }
    o.get("text")?.as_str().map(str::to_string)
}

fn check_parts(
    orig: &Value,
    out: &[Value],
    msg_key: &str,
    owner: &HashMap<&str, usize>,
    msg: usize,
) -> Result<Option<Vec<PartEdit>>, EditError> {
    let empty = Vec::new();
    let inp = orig
        .get("parts")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let keys: HashMap<&str, usize> = inp
        .iter()
        .enumerate()
        .filter_map(|(j, p)| Some((p.get("key")?.as_str()?, j)))
        .collect();
    let mut seen = HashSet::new();
    let mut last: Option<usize> = None;
    let mut changed = false;
    let mut edits = Vec::with_capacity(out.len());
    for (n, p) in out.iter().enumerate() {
        let what = format!("part {n} of message `{msg_key}`");
        let Some(o) = p.as_object() else {
            return Err(bad(format!("{what} is not an object")));
        };
        let Some(kind) = o.get("type").and_then(Value::as_str) else {
            return Err(bad(format!("{what} has no `type`")));
        };
        match o.get("key") {
            Some(Value::String(key)) => {
                let Some(&j) = keys.get(key.as_str()) else {
                    return Err(bad(match owner.get(key.as_str()) {
                        Some(&other) if other != msg => format!(
                            "part `{key}` belongs to another message; parts cannot move between messages"
                        ),
                        _ => format!("{what} has an unknown key `{key}`"),
                    }));
                };
                if !seen.insert(j) {
                    return Err(bad(format!("the key `{key}` appears twice")));
                }
                if last.is_some_and(|l| j < l) {
                    return Err(bad(format!(
                        "part `{key}` moved; kept parts stay in their original order"
                    )));
                }
                last = Some(j);
                let before = &inp[j];
                if before.get("type").and_then(Value::as_str) != Some(kind) {
                    return Err(denied(format!("the type of part `{key}` is read-only")));
                }
                let change = match kind {
                    "text" => {
                        only_fields(o, &["key", "type", "text"], &format!("part `{key}`"))?;
                        let Some(t) = o.get("text").and_then(Value::as_str) else {
                            return Err(bad(format!("part `{key}`: `text` must be a string")));
                        };
                        (before.get("text").and_then(Value::as_str) != Some(t))
                            .then(|| Change::Text(t.to_string()))
                    }
                    "tool_call" => {
                        only_fields(
                            o,
                            &["key", "type", "id", "name", "input"],
                            &format!("part `{key}`"),
                        )?;
                        if o.get("id") != before.get("id") || o.get("name") != before.get("name") {
                            return Err(denied(format!(
                                "`id` and `name` of tool call `{key}` are read-only"
                            )));
                        }
                        let Some(input) = o.get("input") else {
                            return Err(bad(format!("tool call `{key}` has no `input`")));
                        };
                        (Some(input) != before.get("input")).then(|| Change::Input(input.clone()))
                    }
                    "tool_result" => {
                        only_fields(
                            o,
                            &["key", "type", "call_id", "text", "is_error"],
                            &format!("part `{key}`"),
                        )?;
                        if o.get("call_id") != before.get("call_id")
                            || o.get("is_error") != before.get("is_error")
                        {
                            return Err(denied(format!(
                                "`call_id` and `is_error` of tool result `{key}` are read-only"
                            )));
                        }
                        let Some(t) = o.get("text").and_then(Value::as_str) else {
                            return Err(bad(format!("part `{key}`: `text` must be a string")));
                        };
                        (before.get("text").and_then(Value::as_str) != Some(t))
                            .then(|| Change::Result(t.to_string()))
                    }
                    // 推理、图片、别的：整个只读
                    _ => {
                        if p != before {
                            return Err(denied(format!("part `{key}` ({kind}) is read-only")));
                        }
                        None
                    }
                };
                changed |= change.is_some();
                edits.push(PartEdit::Keep { from: j, change });
            }
            Some(_) => return Err(bad(format!("{what}: `key` must be a string"))),
            None => {
                let Some(t) = new_text(p) else {
                    return Err(bad(format!(
                        "{what} is new; only text parts ({{\"type\": \"text\", \"text\": \"…\"}}) can be added"
                    )));
                };
                changed = true;
                edits.push(PartEdit::Insert(t));
            }
        }
    }
    changed |= seen.len() != inp.len();
    Ok(changed.then_some(edits))
}

fn check_tools(
    input: &Value,
    out: &Value,
    hidden: &[String],
) -> Result<Option<Vec<ToolEdit>>, EditError> {
    let empty = Vec::new();
    let inp = input
        .get("tools")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let Some(out) = out.as_array() else {
        return Err(bad("`tools` must be an array"));
    };
    let keys: HashMap<&str, usize> = inp
        .iter()
        .enumerate()
        .filter_map(|(i, t)| Some((t.get("key")?.as_str()?, i)))
        .collect();
    let mut names: HashSet<String> = hidden.iter().cloned().collect();
    let mut seen = HashSet::new();
    let mut last: Option<usize> = None;
    let mut changed = false;
    let mut edits = Vec::with_capacity(out.len());
    for (n, t) in out.iter().enumerate() {
        let Some(o) = t.as_object() else {
            return Err(bad(format!("tools[{n}] is not an object")));
        };
        only_fields(
            o,
            &["key", "name", "description", "input_schema"],
            &format!("tools[{n}]"),
        )?;
        let Some(name) = o
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            return Err(bad(format!("tools[{n}].name must be a non-empty string")));
        };
        let Some(description) = o.get("description").and_then(Value::as_str) else {
            return Err(bad(format!("tools[{n}].description must be a string")));
        };
        let Some(schema) = o.get("input_schema").filter(|s| s.is_object()) else {
            return Err(bad(format!("tools[{n}].input_schema must be an object")));
        };
        if !names.insert(name.to_string()) {
            return Err(bad(format!("two tools are named `{name}`")));
        }
        match o.get("key") {
            Some(Value::String(key)) => {
                let Some(&i) = keys.get(key.as_str()) else {
                    return Err(bad(format!("tools[{n}] has an unknown key `{key}`")));
                };
                if !seen.insert(i) {
                    return Err(bad(format!("the key `{key}` appears twice")));
                }
                if last.is_some_and(|l| i < l) {
                    return Err(bad(format!(
                        "tool `{key}` moved; kept tools stay in their original order"
                    )));
                }
                last = Some(i);
                let before = &inp[i];
                if before.get("name").and_then(Value::as_str) != Some(name) {
                    return Err(denied(format!("the name of tool `{key}` is read-only")));
                }
                let description = (before.get("description").and_then(Value::as_str)
                    != Some(description))
                .then(|| description.to_string());
                let schema = (before.get("input_schema") != Some(schema)).then(|| schema.clone());
                changed |= description.is_some() || schema.is_some();
                edits.push(ToolEdit::Keep {
                    from: i,
                    description,
                    schema,
                });
            }
            Some(_) => return Err(bad(format!("tools[{n}].key must be a string"))),
            None => {
                changed = true;
                edits.push(ToolEdit::Insert {
                    name: name.to_string(),
                    description: description.to_string(),
                    schema: schema.clone(),
                });
            }
        }
    }
    changed |= seen.len() != inp.len();
    Ok(changed.then_some(edits))
}

fn check_params(input: &Value, out: &Value) -> Result<ParamsEdit, EditError> {
    let Some(o) = out.as_object() else {
        return Err(bad("`params` must be an object"));
    };
    only_fields(
        o,
        &["model", "max_tokens", "temperature", "top_p", "stop"],
        "`params`",
    )?;
    let before = input.get("params").unwrap_or(&Value::Null);
    let mut p = ParamsEdit::default();
    let Some(model) = o
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
    else {
        return Err(bad("`params.model` must be a non-empty string"));
    };
    if before.get("model").and_then(Value::as_str) != Some(model) {
        p.model = Some(model.to_string());
    }
    let max = match o.get("max_tokens") {
        None => None,
        Some(v) => match v.as_u64().or_else(|| {
            v.as_f64()
                .filter(|f| f.fract() == 0.0 && *f >= 1.0 && *f <= u64::MAX as f64)
                .map(|f| f as u64)
        }) {
            Some(n) if n >= 1 => Some(n),
            _ => return Err(bad("`params.max_tokens` must be a positive integer")),
        },
    };
    if max != before.get("max_tokens").and_then(Value::as_u64) {
        p.max_tokens = Some(max);
    }
    for (key, slot) in [("temperature", &mut p.temperature), ("top_p", &mut p.top_p)] {
        let v = match o.get(key) {
            None => None,
            Some(v) => match v.as_f64().filter(|f| f.is_finite()) {
                Some(f) => Some(f),
                None => return Err(bad(format!("`params.{key}` must be a number"))),
            },
        };
        if v != before.get(key).and_then(Value::as_f64) {
            *slot = Some(v);
        }
    }
    let stop = match o.get("stop") {
        None => None,
        Some(Value::Array(a)) => {
            let mut s = Vec::with_capacity(a.len());
            for x in a {
                match x.as_str() {
                    Some(t) => s.push(t.to_string()),
                    None => return Err(bad("`params.stop` must be an array of strings")),
                }
            }
            Some(s)
        }
        Some(_) => return Err(bad("`params.stop` must be an array of strings")),
    };
    let had: Option<Vec<String>> = before.get("stop").and_then(Value::as_array).map(|a| {
        a.iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    });
    if stop != had {
        p.stop = Some(stop);
    }
    Ok(p)
}

// ───────────────────────────────────────────────────────── 写回用的小工具

/// 视图里一个部分。
pub(crate) fn part_text(key: &str, text: &str) -> Value {
    json!({ "key": key, "type": "text", "text": text })
}

pub(crate) fn part_thinking(key: &str, text: &str) -> Value {
    json!({ "key": key, "type": "thinking", "text": text })
}

pub(crate) fn part_call(key: &str, id: &str, name: &str, input: Value) -> Value {
    json!({ "key": key, "type": "tool_call", "id": id, "name": name, "input": input })
}

pub(crate) fn part_result(key: &str, call_id: &str, text: &str, is_error: bool) -> Value {
    json!({ "key": key, "type": "tool_result", "call_id": call_id, "text": text, "is_error": is_error })
}

pub(crate) fn part_image(key: &str, media_type: Option<&str>) -> Value {
    json!({ "key": key, "type": "image", "media_type": media_type })
}

pub(crate) fn part_other(key: &str, label: &str) -> Value {
    json!({ "key": key, "type": "other", "label": label })
}

pub(crate) fn msg_key(i: usize) -> String {
    format!("m{i}")
}

pub(crate) fn part_key(i: usize, j: usize) -> String {
    format!("m{i}.p{j}")
}

pub(crate) fn tool_key(i: usize) -> String {
    format!("t{i}")
}

/// `data:image/png;base64,…` 里的类型。不是 data URI 的不知道
pub(crate) fn data_uri_mime(uri: &str) -> Option<&str> {
    let rest = uri.strip_prefix("data:")?;
    let (head, _) = rest.split_once(',')?;
    Some(head.split(';').next().unwrap_or(head)).filter(|m| !m.is_empty())
}

/// 工具参数的 JSON 文本读成值。**读不开的原文当字符串**，和转换时一样：丢掉参数比
/// 一个形状奇怪的参数糟得多
pub(crate) fn args_value(text: &str) -> Value {
    if text.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

/// 参数值写回成 JSON 文本：字符串原样（它本来就是读不开的原文），别的序列化
pub(crate) fn args_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// 一个数组里有几项看得见、几项看不见（服务端工具、隐藏的块）时，按插件交回来的
/// 顺序重排：看不见的留在原位，留下来的按新的样子，删掉的去掉，新加的插在它后面那个
/// 留下来的项之前（后面没有就放到最后）。
///
/// `visible[k]` 是视图里第 k 项在原数组里的下标；`out` 是插件的顺序：`Ok((k, 新值))`
/// 是留下的第 k 项，`Err(新值)` 是新加的。
pub(crate) fn merge(
    original: &[Value],
    visible: &[usize],
    out: Vec<Result<(usize, Value), Value>>,
) -> Vec<Value> {
    let slot: HashMap<usize, usize> = visible.iter().enumerate().map(|(k, &i)| (i, k)).collect();
    let mut kept: HashMap<usize, usize> = HashMap::new();
    for (pos, o) in out.iter().enumerate() {
        if let Ok((k, _)) = o {
            kept.insert(*k, pos);
        }
    }
    let mut out: Vec<Option<Result<(usize, Value), Value>>> = out.into_iter().map(Some).collect();
    let mut next = 0usize;
    let mut result = Vec::with_capacity(original.len() + out.len());
    for (i, item) in original.iter().enumerate() {
        let Some(&k) = slot.get(&i) else {
            result.push(item.clone());
            continue;
        };
        let Some(&pos) = kept.get(&k) else {
            // 删掉了
            continue;
        };
        // 排在它前面的新项先放
        while next < pos {
            if let Some(Err(v)) = out[next].take() {
                result.push(v);
            }
            next += 1;
        }
        if let Some(Ok((_, v))) = out[pos].take() {
            result.push(v);
        }
        next = pos + 1;
    }
    for o in out.into_iter().skip(next).flatten() {
        match o {
            Err(v) | Ok((_, v)) => result.push(v),
        }
    }
    result
}

#[cfg(test)]
mod tests;
