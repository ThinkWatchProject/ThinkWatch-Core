//! 四种客户端格式里的消息怎么读成对话里的一块一块。
//!
//! **不经过 tw-dialect 的中间表示。**中间表示只装四种格式之间对应得上的东西：对话中途的
//! 系统消息并进了系统提示，只装着工具结果的消息和用户说的话是同一个角色，服务端工具的
//! 调用和结果、文件、音频解码时直接丢掉 —— 转换格式时那样是对的，可对话记录要的恰恰是
//! 这些：谁在哪一处说了什么，哪里有一块转不过去的东西。所以这里照客户端自己的格式读
//! JSON，只读消息这一层；取哪个字段和 tw-dialect 的解码器保持一致。
//!
//! 整包的回答和请求里的消息是同一个形状（Anthropic 的 `content`、Chat 的 `message`、
//! Responses 的 `output`、Gemini 的 `content`），也按这里读。

use std::borrow::Cow;

use serde_json::Value;
use tw_dialect::ir::Dialect;

/// 一条消息是谁说的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Role {
    User,
    Assistant,
    /// 只装着工具结果
    Tool,
    /// 对话中途的 system、developer 消息
    System,
}

/// 读出来的一块，借着解析好的 JSON。
///
/// **已经是规整过的样子**：缓存断点和推理签名不在里面，键的顺序也无所谓了 —— 前后两个
/// 请求比对时，这些本来就不该算（Claude Code 每一轮都把缓存断点挪到最后一条消息上）。
#[derive(Debug, Clone)]
pub(super) enum Piece<'a> {
    Text(Cow<'a, str>),
    /// 推理。只有签名、被打码的，是空串
    Thinking(Cow<'a, str>),
    ToolCall {
        id: Cow<'a, str>,
        name: Cow<'a, str>,
        input: Input<'a>,
    },
    ToolResult {
        call_id: Cow<'a, str>,
        text: Cow<'a, str>,
        is_error: bool,
    },
    /// `data` 只进指纹，**从不交出去**
    Image {
        media_type: Option<&'a str>,
        bytes: Option<u64>,
        data: Option<&'a str>,
    },
    /// 别的块，类型名原样
    Other(&'a str),
}

/// 工具调用的参数。
#[derive(Debug, Clone)]
pub(super) enum Input<'a> {
    /// 函数工具的参数，模型写出来的 JSON 文本。空的当 `{}`
    Args(&'a str),
    /// 自由格式工具的原文（Codex 的 `apply_patch`），一个字不改
    Raw(&'a str),
    /// 已经是 JSON 的参数（Anthropic、Gemini）
    Json(&'a Value),
}

impl Input<'_> {
    /// 交出去的样子：JSON 文本，自由格式的是原文
    pub(super) fn text(&self) -> String {
        match self {
            Input::Args(s) if s.trim().is_empty() => "{}".into(),
            Input::Args(s) | Input::Raw(s) => s.to_string(),
            Input::Json(v) => v.to_string(),
        }
    }
}

/// 一条消息（Responses 里是一个输入项）读出来的样子。
#[derive(Debug, Clone)]
pub(super) struct Item<'a> {
    pub(super) role: Role,
    pub(super) pieces: Vec<Piece<'a>>,
}

/// 一个请求体里对话记录要的东西。
#[derive(Debug, Default)]
pub(super) struct Body<'a> {
    /// 系统提示，按出现的先后
    pub(super) system: Vec<Cow<'a, str>>,
    pub(super) items: Vec<Item<'a>>,
    /// 定义成自由格式的工具。别的格式的上游会把它们的原文包进 `{"input": …}`
    pub(super) freeform: Vec<String>,
}

/// 按客户端的格式读一个请求体。不是对象、没有对话的读出来是空的，不算读不懂。
pub(super) fn body(dialect: Dialect, v: &Value) -> Body<'_> {
    match dialect {
        Dialect::Anthropic => anthropic(v),
        Dialect::Chat => chat(v),
        Dialect::Responses => responses(v),
        Dialect::Gemini => gemini(v),
        // 客户端不说 Converse（见 `tw_api::Dialect::Bedrock`）
        Dialect::Bedrock => Body::default(),
    }
}

// ───────────────────────────────────────────────────────── 小工具

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn arr_of<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// 一个 `string | [{type: "text", text}]` 形状的内容里的字，几段之间换行。
fn text_of(v: &Value) -> Cow<'_, str> {
    match v {
        Value::String(s) => Cow::Borrowed(s),
        Value::Array(parts) => {
            let texts: Vec<&str> = parts.iter().filter_map(|p| str_of(p, "text")).collect();
            match texts.as_slice() {
                [] => Cow::Borrowed(""),
                [one] => Cow::Borrowed(one),
                many => Cow::Owned(many.join("\n")),
            }
        }
        _ => Cow::Borrowed(""),
    }
}

/// 块的类型名。没写的叫 `unknown`
fn kind_of(v: &Value) -> &str {
    str_of(v, "type").unwrap_or("unknown")
}

/// base64 解出来有多少字节
fn base64_len(data: &str) -> u64 {
    let d = data.trim_end();
    let pad = d.bytes().rev().take_while(|b| *b == b'=').count() as u64;
    (d.len() as u64 * 3 / 4).saturating_sub(pad)
}

/// 一张以 URI 给的图片：`data:` URI 说得出类型和大小，网址说不出
fn image_uri(uri: &str) -> Piece<'_> {
    if let Some(rest) = uri.strip_prefix("data:")
        && let Some((head, data)) = rest.split_once(',')
    {
        let (mime, base64) = match head.strip_suffix(";base64") {
            Some(m) => (m, true),
            None => (head.split(';').next().unwrap_or_default(), false),
        };
        return Piece::Image {
            media_type: (!mime.is_empty()).then_some(mime),
            bytes: base64.then(|| base64_len(data)),
            data: Some(data),
        };
    }
    Piece::Image {
        media_type: None,
        bytes: None,
        data: Some(uri),
    }
}

/// 文字，空的不算一块
fn text_piece<'a>(t: Cow<'a, str>, out: &mut Vec<Piece<'a>>) {
    if !t.is_empty() {
        out.push(Piece::Text(t));
    }
}

// ───────────────────────────────────────────────────────── Anthropic Messages

fn anthropic(v: &Value) -> Body<'_> {
    let mut system = Vec::new();
    match v.get("system") {
        Some(Value::String(s)) => system.push(Cow::Borrowed(s.as_str())),
        Some(Value::Array(blocks)) => system.extend(
            blocks
                .iter()
                .filter_map(|b| str_of(b, "text"))
                .map(Cow::Borrowed),
        ),
        _ => {}
    }
    Body {
        system,
        items: arr_of(v, "messages")
            .iter()
            .map(anthropic_message)
            .collect(),
        freeform: Vec::new(),
    }
}

fn anthropic_message(m: &Value) -> Item<'_> {
    let mut pieces = Vec::new();
    let mut only_results = false;
    match m.get("content") {
        Some(Value::String(s)) => text_piece(Cow::Borrowed(s), &mut pieces),
        Some(Value::Array(blocks)) => {
            only_results = !blocks.is_empty()
                && blocks
                    .iter()
                    .all(|b| str_of(b, "type") == Some("tool_result"));
            anthropic_blocks(blocks, &mut pieces);
        }
        _ => {}
    }
    let role = match str_of(m, "role") {
        Some("assistant") => Role::Assistant,
        // DeepSeek Harness 在消息里放的 system 角色
        Some("system") => Role::System,
        _ if only_results => Role::Tool,
        _ => Role::User,
    };
    Item { role, pieces }
}

/// 一串内容块：消息的 `content`，也是整包回答的 `content`。
pub(super) fn anthropic_blocks<'a>(blocks: &'a [Value], out: &mut Vec<Piece<'a>>) {
    for b in blocks {
        match kind_of(b) {
            "text" => text_piece(Cow::Borrowed(str_of(b, "text").unwrap_or_default()), out),
            "thinking" => out.push(Piece::Thinking(Cow::Borrowed(
                str_of(b, "thinking").unwrap_or_default(),
            ))),
            "redacted_thinking" => out.push(Piece::Thinking(Cow::Borrowed(""))),
            "tool_use" => out.push(Piece::ToolCall {
                id: Cow::Borrowed(str_of(b, "id").unwrap_or_default()),
                name: Cow::Borrowed(str_of(b, "name").unwrap_or_default()),
                input: b.get("input").map_or(Input::Args(""), Input::Json),
            }),
            "tool_result" => anthropic_result(b, out),
            "image" => out.push(anthropic_image(b)),
            // document、search_result，服务端工具的 server_tool_use、web_search_tool_result……
            other => out.push(Piece::Other(other)),
        }
    }
}

fn anthropic_image(b: &Value) -> Piece<'_> {
    let src = b.get("source").unwrap_or(&Value::Null);
    match str_of(src, "type") {
        Some("base64") => {
            let data = str_of(src, "data").unwrap_or_default();
            Piece::Image {
                media_type: str_of(src, "media_type"),
                bytes: Some(base64_len(data)),
                data: Some(data),
            }
        }
        Some("url") => Piece::Image {
            media_type: None,
            bytes: None,
            data: str_of(src, "url"),
        },
        // Files API 里的文件：只有一个文件号
        _ => Piece::Image {
            media_type: None,
            bytes: None,
            data: str_of(src, "file_id"),
        },
    }
}

/// 工具结果：里面的字连起来成一块，图片和别的块跟在它后面。
fn anthropic_result<'a>(b: &'a Value, out: &mut Vec<Piece<'a>>) {
    let mut extra = Vec::new();
    let text = match b.get("content") {
        Some(Value::String(s)) => Cow::Borrowed(s.as_str()),
        Some(Value::Array(items)) => {
            let mut texts = Vec::new();
            for i in items {
                match kind_of(i) {
                    "text" => texts.push(str_of(i, "text").unwrap_or_default()),
                    "image" => extra.push(anthropic_image(i)),
                    other => extra.push(Piece::Other(other)),
                }
            }
            match texts.as_slice() {
                [one] => Cow::Borrowed(*one),
                many => Cow::Owned(many.join("\n")),
            }
        }
        _ => Cow::Borrowed(""),
    };
    out.push(Piece::ToolResult {
        call_id: Cow::Borrowed(str_of(b, "tool_use_id").unwrap_or_default()),
        text,
        is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
    });
    out.extend(extra);
}

// ───────────────────────────────────────────────────────── OpenAI Chat Completions

fn chat(v: &Value) -> Body<'_> {
    let mut system = Vec::new();
    let mut items = Vec::new();
    // 开头连着的 system、developer 消息就是系统提示；对话中途的照原位置留着
    let mut leading = true;
    for m in arr_of(v, "messages") {
        let role = str_of(m, "role").unwrap_or_default();
        if leading && matches!(role, "system" | "developer") {
            system.push(text_of(m.get("content").unwrap_or(&Value::Null)));
            continue;
        }
        leading = false;
        items.push(chat_message(m));
    }
    Body {
        system,
        items,
        freeform: arr_of(v, "tools")
            .iter()
            .filter(|t| str_of(t, "type") == Some("custom"))
            .filter_map(|t| t.get("custom").and_then(|c| str_of(c, "name")))
            .map(str::to_string)
            .collect(),
    }
}

fn chat_message(m: &Value) -> Item<'_> {
    let content = m.get("content").unwrap_or(&Value::Null);
    match str_of(m, "role").unwrap_or_default() {
        "system" | "developer" => {
            let mut pieces = Vec::new();
            text_piece(text_of(content), &mut pieces);
            Item {
                role: Role::System,
                pieces,
            }
        }
        "assistant" => Item {
            role: Role::Assistant,
            pieces: chat_assistant(m),
        },
        "tool" => Item {
            role: Role::Tool,
            pieces: vec![Piece::ToolResult {
                call_id: Cow::Borrowed(str_of(m, "tool_call_id").unwrap_or_default()),
                text: text_of(content),
                is_error: false,
            }],
        },
        // 旧的函数调用：结果按函数名认
        "function" => Item {
            role: Role::Tool,
            pieces: vec![Piece::ToolResult {
                call_id: Cow::Borrowed(str_of(m, "name").unwrap_or_default()),
                text: text_of(content),
                is_error: false,
            }],
        },
        _ => {
            let mut pieces = Vec::new();
            match content {
                Value::String(s) => text_piece(Cow::Borrowed(s), &mut pieces),
                Value::Array(parts) => {
                    for p in parts {
                        match kind_of(p) {
                            "text" => text_piece(
                                Cow::Borrowed(str_of(p, "text").unwrap_or_default()),
                                &mut pieces,
                            ),
                            "image_url" => {
                                let url = p
                                    .get("image_url")
                                    .and_then(|i| i.as_str().or_else(|| str_of(i, "url")));
                                pieces.push(url.map_or(
                                    Piece::Image {
                                        media_type: None,
                                        bytes: None,
                                        data: None,
                                    },
                                    image_uri,
                                ));
                            }
                            // input_audio、file……
                            other => pieces.push(Piece::Other(other)),
                        }
                    }
                }
                _ => {}
            }
            Item {
                role: Role::User,
                pieces,
            }
        }
    }
}

/// 助手消息：请求里的历史，也是整包回答的 `choices[0].message`。
pub(super) fn chat_assistant(m: &Value) -> Vec<Piece<'_>> {
    let mut pieces = Vec::new();
    // DeepSeek、Kimi、GLM 这些的推理字段（OpenRouter 叫 `reasoning`）
    if let Some(t) = str_of(m, "reasoning_content")
        .or_else(|| str_of(m, "reasoning"))
        .filter(|t| !t.is_empty())
    {
        pieces.push(Piece::Thinking(Cow::Borrowed(t)));
    }
    match m.get("content") {
        Some(Value::String(s)) => text_piece(Cow::Borrowed(s), &mut pieces),
        Some(Value::Array(parts)) => {
            for p in parts {
                match str_of(p, "text").or_else(|| str_of(p, "refusal")) {
                    Some(t) => text_piece(Cow::Borrowed(t), &mut pieces),
                    None => pieces.push(Piece::Other(kind_of(p))),
                }
            }
        }
        _ => {}
    }
    if let Some(r) = str_of(m, "refusal") {
        text_piece(Cow::Borrowed(r), &mut pieces);
    }
    for c in arr_of(m, "tool_calls") {
        let id = Cow::Borrowed(str_of(c, "id").unwrap_or_default());
        match str_of(c, "type") {
            Some("custom") => {
                let x = c.get("custom").unwrap_or(&Value::Null);
                pieces.push(Piece::ToolCall {
                    id,
                    name: Cow::Borrowed(str_of(x, "name").unwrap_or_default()),
                    input: Input::Raw(str_of(x, "input").unwrap_or_default()),
                });
            }
            _ => {
                let f = c.get("function").unwrap_or(&Value::Null);
                pieces.push(Piece::ToolCall {
                    id,
                    name: Cow::Borrowed(str_of(f, "name").unwrap_or_default()),
                    input: Input::Args(str_of(f, "arguments").unwrap_or_default()),
                });
            }
        }
    }
    // 旧的函数调用：没有调用号
    if let Some(f) = m.get("function_call") {
        pieces.push(Piece::ToolCall {
            id: Cow::Borrowed(""),
            name: Cow::Borrowed(str_of(f, "name").unwrap_or_default()),
            input: Input::Args(str_of(f, "arguments").unwrap_or_default()),
        });
    }
    if m.get("audio").is_some_and(|a| !a.is_null()) {
        pieces.push(Piece::Other("audio"));
    }
    pieces
}

// ───────────────────────────────────────────────────────── OpenAI Responses

fn responses(v: &Value) -> Body<'_> {
    let mut system = Vec::new();
    match v.get("instructions") {
        Some(Value::String(s)) => system.push(Cow::Borrowed(s.as_str())),
        Some(a @ Value::Array(_)) => system.push(text_of(a)),
        _ => {}
    }
    let mut items = Vec::new();
    match v.get("input") {
        Some(Value::String(s)) => {
            let mut pieces = Vec::new();
            text_piece(Cow::Borrowed(s), &mut pieces);
            items.push(Item {
                role: Role::User,
                pieces,
            });
        }
        Some(Value::Array(input)) => {
            // 开头连着的 system、developer 消息算系统提示，和 Chat 一样
            let mut leading = true;
            for it in input {
                if leading
                    && str_of(it, "type").unwrap_or("message") == "message"
                    && matches!(str_of(it, "role"), Some("system" | "developer"))
                {
                    system.push(text_of(it.get("content").unwrap_or(&Value::Null)));
                    continue;
                }
                leading = false;
                items.push(responses_item(it));
            }
        }
        _ => {}
    }
    let mut freeform = Vec::new();
    for t in arr_of(v, "tools") {
        match str_of(t, "type") {
            Some("custom") => freeform.extend(str_of(t, "name").map(str::to_string)),
            Some("namespace") => {
                let ns = str_of(t, "name");
                freeform.extend(
                    arr_of(t, "tools")
                        .iter()
                        .filter(|x| str_of(x, "type") == Some("custom"))
                        .filter_map(|x| str_of(x, "name"))
                        .map(|n| flat_name(ns, n).into_owned()),
                );
            }
            _ => {}
        }
    }
    Body {
        system,
        items,
        freeform,
    }
}

/// namespace 里的工具展开成 `namespace__名字`，和转换给别家时的名字一样
fn flat_name<'a>(namespace: Option<&str>, name: &'a str) -> Cow<'a, str> {
    match namespace {
        Some(ns) if !ns.is_empty() => Cow::Owned(format!("{ns}__{name}")),
        _ => Cow::Borrowed(name),
    }
}

/// 一个输入项：请求里的历史，也是整包回答的 `output` 里的一项。
pub(super) fn responses_item(it: &Value) -> Item<'_> {
    let kind = str_of(it, "type").unwrap_or("message");
    let one = |role, piece| Item {
        role,
        pieces: vec![piece],
    };
    match kind {
        "message" => {
            let role = match str_of(it, "role") {
                Some("assistant") => Role::Assistant,
                Some("system" | "developer") => Role::System,
                _ => Role::User,
            };
            let mut pieces = Vec::new();
            match it.get("content") {
                Some(Value::String(s)) => text_piece(Cow::Borrowed(s), &mut pieces),
                Some(Value::Array(parts)) => responses_parts(parts, &mut pieces),
                _ => {}
            }
            Item { role, pieces }
        }
        "reasoning" => {
            let texts = |key: &str| -> Vec<&str> {
                arr_of(it, key)
                    .iter()
                    .filter_map(|x| str_of(x, "text"))
                    .collect()
            };
            // 推理原文优先，没有就是摘要；几段之间空一行（和 tw-dialect 一样）
            let mut t = texts("content");
            if t.is_empty() {
                t = texts("summary");
            }
            let text = match t.as_slice() {
                [] => Cow::Borrowed(""),
                [one] => Cow::Borrowed(*one),
                many => Cow::Owned(many.join("\n\n")),
            };
            one(Role::Assistant, Piece::Thinking(text))
        }
        "function_call" | "custom_tool_call" => {
            let name = flat_name(
                str_of(it, "namespace"),
                str_of(it, "name").unwrap_or_default(),
            );
            let input = if kind == "function_call" {
                Input::Args(str_of(it, "arguments").unwrap_or_default())
            } else {
                Input::Raw(str_of(it, "input").unwrap_or_default())
            };
            one(
                Role::Assistant,
                Piece::ToolCall {
                    id: Cow::Borrowed(str_of(it, "call_id").unwrap_or_default()),
                    name,
                    input,
                },
            )
        }
        "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
            let mut extra = Vec::new();
            let text = match it.get("output") {
                Some(Value::String(s)) => Cow::Borrowed(s.as_str()),
                Some(Value::Array(parts)) => {
                    let mut texts = Vec::new();
                    for p in parts {
                        match kind_of(p) {
                            "input_text" | "output_text" => {
                                texts.push(str_of(p, "text").unwrap_or_default())
                            }
                            "input_image" => extra.push(responses_image(p)),
                            other => extra.push(Piece::Other(other)),
                        }
                    }
                    match texts.as_slice() {
                        [one] => Cow::Borrowed(*one),
                        many => Cow::Owned(many.join("\n")),
                    }
                }
                _ => Cow::Borrowed(""),
            };
            let mut pieces = vec![Piece::ToolResult {
                call_id: Cow::Borrowed(str_of(it, "call_id").unwrap_or_default()),
                text,
                is_error: false,
            }];
            pieces.extend(extra);
            Item {
                role: Role::Tool,
                pieces,
            }
        }
        // 压缩过的前文：只有 OpenAI 读得懂的一段密文，在对话里的位置像一条系统消息
        "compaction" => one(Role::System, Piece::Other(kind)),
        // 别的工具结果（computer_call_output……）
        k if k.ends_with("_output") => one(Role::Tool, Piece::Other(k)),
        // 托管工具的调用（web_search_call、image_generation_call、mcp_call、local_shell_call……）
        k if k.ends_with("_call") || matches!(k, "mcp_list_tools" | "mcp_approval_request") => {
            one(Role::Assistant, Piece::Other(k))
        }
        // item_reference、mcp_approval_response……
        other => one(Role::User, Piece::Other(other)),
    }
}

fn responses_parts<'a>(parts: &'a [Value], out: &mut Vec<Piece<'a>>) {
    for p in parts {
        match kind_of(p) {
            "input_text" | "output_text" => {
                text_piece(Cow::Borrowed(str_of(p, "text").unwrap_or_default()), out)
            }
            "refusal" => text_piece(Cow::Borrowed(str_of(p, "refusal").unwrap_or_default()), out),
            "input_image" => out.push(responses_image(p)),
            // input_file、input_audio……
            other => out.push(Piece::Other(other)),
        }
    }
}

fn responses_image(p: &Value) -> Piece<'_> {
    match str_of(p, "image_url") {
        Some(u) => image_uri(u),
        None => Piece::Image {
            media_type: None,
            bytes: None,
            data: str_of(p, "file_id"),
        },
    }
}

// ───────────────────────────────────────────────────────── Gemini

/// 按驼峰名取字段，取不到再试下划线写法：Gemini 的 REST 接口两种都收
fn field<'a>(v: &'a Value, camel: &str) -> Option<&'a Value> {
    v.get(camel).or_else(|| {
        let mut snake = String::with_capacity(camel.len() + 4);
        for c in camel.chars() {
            if c.is_ascii_uppercase() {
                snake.push('_');
                snake.push(c.to_ascii_lowercase());
            } else {
                snake.push(c);
            }
        }
        v.get(snake)
    })
}

fn fstr<'a>(v: &'a Value, camel: &str) -> Option<&'a str> {
    field(v, camel).and_then(Value::as_str)
}

fn gemini(v: &Value) -> Body<'_> {
    let mut system = Vec::new();
    match field(v, "systemInstruction") {
        Some(Value::String(s)) => system.push(Cow::Borrowed(s.as_str())),
        Some(sys) => system.push(text_of(sys.get("parts").unwrap_or(&Value::Null))),
        None => {}
    }
    Body {
        system,
        items: arr_of(v, "contents").iter().map(gemini_content).collect(),
        freeform: Vec::new(),
    }
}

fn gemini_content(c: &Value) -> Item<'_> {
    let parts = arr_of(c, "parts");
    let mut pieces = Vec::new();
    gemini_parts(parts, &mut pieces);
    let role = match str_of(c, "role") {
        Some("model") => Role::Assistant,
        Some("function") => Role::Tool,
        Some("system") => Role::System,
        _ if !parts.is_empty() && parts.iter().all(|p| field(p, "functionResponse").is_some()) => {
            Role::Tool
        }
        _ => Role::User,
    };
    Item { role, pieces }
}

/// 一串 part：请求里的一条消息，也是回答里候选的 `content.parts`。
///
/// 没有调用号的函数调用和结果按函数名对上（Gemini 自己不要求调用号）。
pub(super) fn gemini_parts<'a>(parts: &'a [Value], out: &mut Vec<Piece<'a>>) {
    for p in parts {
        if let Some(t) = fstr(p, "text") {
            if p.get("thought").and_then(Value::as_bool) == Some(true) {
                out.push(Piece::Thinking(Cow::Borrowed(t)));
            } else {
                text_piece(Cow::Borrowed(t), out);
            }
        } else if let Some(blob) = field(p, "inlineData") {
            let mime = fstr(blob, "mimeType").unwrap_or_default();
            let data = fstr(blob, "data").unwrap_or_default();
            out.push(if mime.starts_with("image/") {
                Piece::Image {
                    media_type: Some(mime),
                    bytes: Some(base64_len(data)),
                    data: Some(data),
                }
            } else {
                Piece::Other("inlineData")
            });
        } else if let Some(file) = field(p, "fileData") {
            let mime = fstr(file, "mimeType").unwrap_or_default();
            out.push(if mime.starts_with("image/") {
                Piece::Image {
                    media_type: Some(mime),
                    bytes: None,
                    data: fstr(file, "fileUri"),
                }
            } else {
                Piece::Other("fileData")
            });
        } else if let Some(call) = field(p, "functionCall") {
            let name = fstr(call, "name").unwrap_or_default();
            out.push(Piece::ToolCall {
                id: Cow::Borrowed(fstr(call, "id").unwrap_or(name)),
                name: Cow::Borrowed(name),
                input: call.get("args").map_or(Input::Args(""), Input::Json),
            });
        } else if let Some(resp) = field(p, "functionResponse") {
            let name = fstr(resp, "name").unwrap_or_default();
            let body = resp.get("response").unwrap_or(&Value::Null);
            out.push(Piece::ToolResult {
                call_id: Cow::Borrowed(fstr(resp, "id").unwrap_or(name)),
                text: gemini_response_text(body),
                is_error: body.get("error").is_some() && body.get("output").is_none(),
            });
        } else if field(p, "executableCode").is_some() {
            out.push(Piece::Other("executableCode"));
        } else if field(p, "codeExecutionResult").is_some() {
            out.push(Piece::Other("codeExecutionResult"));
        }
        // 只带着推理签名的空块、视频片段的说明这些：没有可看的
    }
}

/// 函数结果写成字：只有一项 `output`（或 `result`、`content`、`error`）且是字符串的取那个
/// 字符串，别的写成 JSON。和 tw-dialect 转给别家时的写法一样
fn gemini_response_text(v: &Value) -> Cow<'_, str> {
    if let Some(o) = v.as_object()
        && o.len() == 1
        && let Some(s) = ["output", "result", "content", "error"]
            .iter()
            .find_map(|k| o.get(*k).and_then(Value::as_str))
    {
        return Cow::Borrowed(s);
    }
    match v {
        Value::String(s) => Cow::Borrowed(s),
        Value::Null => Cow::Borrowed(""),
        other => Cow::Owned(other.to_string()),
    }
}
