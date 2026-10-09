//! OpenAI Responses 请求 ⇄ 中间表示。
//!
//! # 输入项转给别家时怎么办
//!
//! 照 Codex 往 `input` 里放的每一种项写（`codex-rs/protocol/src/models.rs` 的
//! `ResponseItem`）。**丢弃只在别家确实没有对应物、模型也不缺什么的时候**，丢了的记进
//! [`Dropped`]：
//!
//! | 输入项 | 转成 |
//! |---|---|
//! | `additional_tools` | 里面的工具和顶层 `tools` 一样解码。Responses Lite 只在这里声明工具，顶层没有 `tools`；对话里可以有好几个（增量声明） |
//! | `message` | 开头连着的 `system`、`developer` 并进系统提示，对话中途的留在原位（[`Role::System`]），别的是一轮对话。`phase`（commentary、final_answer）别家没有，文字照留 |
//! | `agent_message` | 别的代理发来的话（发信人和任务名写在正文里），当用户的一轮。只有 OpenAI 读得懂的 `encrypted_content` 记丢弃 |
//! | `reasoning` | 推理，签名照规矩带着 |
//! | `function_call`、`custom_tool_call` 和各自的 `_output` | 工具调用和结果 |
//! | `local_shell_call` | 老历史里的命令调用：叫 `local_shell` 的工具调用，参数是它的 `action`。结果是同一个 `call_id` 的 `function_call_output`，丢了调用、留着结果，Anthropic 会拒绝整个请求 |
//! | `tool_search_call`、`tool_search_output` | 一次 `tool_search` 调用和结果。搜到的工具从此可以调用，加进工具列表 |
//! | `configuration_update` | 对话中途改的推理强度。最后一个说了算，盖过顶层的 `reasoning.effort` —— Codex 为了保住提示缓存，顶层一直写开头那一档 |
//! | `web_search_call`、`image_generation_call` | 丢弃并记下：OpenAI 服务端工具的执行记录，搜到的、画出的写在后面的回答里 |
//! | `compaction_trigger` | 要压缩前文：历史照转，末尾请上游写一份交接摘要，回答作为一个 `compaction` 项交回（见 [`crate::compaction`]） |
//! | `compaction`、`context_compaction` | 转换写出去的（`tw1.c.`）解回摘要，留在原位；OpenAI 自己的密文拒绝，只有 OpenAI 读得懂 |
//! | `item_reference` | 拒绝：内容在 OpenAI 服务端 |
//!
//! 顶层字段：`text.verbosity` 写给认它的模型（[`Verbosity::understood_by`]），别处记丢弃；
//! `service_tier` 别家没有同样的档位，记丢弃。`store`、`include`（转换写出的推理项总是带着
//! `encrypted_content`）、`prompt_cache_key`、`client_metadata`、`stream_options`、
//! `reasoning.context` 是给 OpenAI 服务端的存储和传输参数，模型看不到，不记 ——
//! `client_metadata` 里是 Codex 的会话信息，本来就不该交给别家。

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::ir::*;
use crate::think;

// ───────────────────────────────────────────────────────── 解码

/// Codex 的默认 namespace。Responses Lite 把顶层的函数和自由格式工具都装在它里面，而
/// Codex 认 `functions` 里的 `shell` 和不带 namespace 的 `shell` 是同一个工具
/// （`codex-rs/protocol/src/tool_name.rs`），所以展开时不加前缀
const DEFAULT_NAMESPACE: &str = "functions";

/// Codex 的工具搜索（`{"type": "tool_search", "execution": "client"}`）没有名字，转给别家时
/// 写成叫这个名字的函数工具
pub const TOOL_SEARCH: &str = "tool_search";

/// `local_shell_call` 转成的工具调用叫什么。Codex 已经不再声明这个工具，它只出现在老历史里
const LOCAL_SHELL: &str = "local_shell";

/// 别家工具名的长度上限：OpenAI Chat、Gemini、Bedrock 都是 64
const NAME_LIMIT: usize = 64;

/// 客户端发来的 Responses 请求 → 中间表示。
///
/// **依赖 OpenAI 服务端状态的请求直接拒绝**（`previous_response_id`、`conversation`、
/// 引用服务端内容的输入项）：对话内容不在请求里，转给别家只会得到一个忘了前文的
/// 回答，而且看起来一切正常。
pub fn decode_request(
    v: &Value,
    dropped: &mut Dropped,
    shape: &mut ClientShape,
) -> Result<Request, Rejection> {
    if !v.is_object() {
        return Err(Rejection("The request body is not a JSON object.".into()));
    }
    for (key, what) in [
        (
            "previous_response_id",
            "the conversation lives on OpenAI's servers",
        ),
        ("conversation", "the conversation lives on OpenAI's servers"),
        ("prompt", "the prompt template lives on OpenAI's servers"),
    ] {
        if has(v, key) {
            return Err(Rejection(format!(
                "The request uses {key}, and {what}, so it cannot be converted for an upstream of another format."
            )));
        }
    }
    if v.get("background").and_then(Value::as_bool) == Some(true) {
        return Err(Rejection(
            "The request uses background, which only OpenAI's servers support, so it cannot be converted for an upstream of another format."
                .into(),
        ));
    }

    let mut r = Request {
        model: str_of(v, "model").unwrap_or_default().to_string(),
        max_tokens: u64_of(v, "max_output_tokens"),
        temperature: f64_of(v, "temperature"),
        top_p: f64_of(v, "top_p"),
        parallel_tool_calls: v.get("parallel_tool_calls").and_then(Value::as_bool),
        stream: v.get("stream").and_then(Value::as_bool).unwrap_or(false),
        ..Default::default()
    };
    if let Some(i) = str_of(v, "instructions").filter(|i| !i.is_empty()) {
        r.system.push(i.to_string());
    }

    let mut cx = Ctx {
        dropped,
        shape,
        tools: ToolSet::default(),
        effort: None,
        compaction: false,
    };
    for t in arr_of(v, "tools") {
        cx.tool(t, None, "tools");
    }
    match v.get("input") {
        Some(Value::String(s)) if !s.is_empty() => r.messages.push(Message {
            role: Role::User,
            parts: vec![Part::Text(s.clone())],
        }),
        Some(Value::Array(items)) => {
            for item in items {
                cx.item(item, &mut r)?;
            }
        }
        _ => {}
    }
    let Ctx {
        dropped,
        shape,
        tools,
        effort,
        compaction,
    } = cx;
    r.tools = tools.tools;
    // 要压缩前文：历史、工具、推理设置都不动（和上一轮同一个开头，提示缓存照样命中），
    // 末尾请上游写摘要
    if compaction {
        shape.compaction = true;
        r.messages.push(Message {
            role: Role::User,
            parts: vec![Part::Text(crate::compaction::INSTRUCTION.to_string())],
        });
    }
    // namespace 的说明（MCP 服务器的使用说明就写在这里）：别家的工具没有 namespace，
    // 写进系统提示，模型照样看得到
    for (ns, note) in tools.notes {
        r.system.push(if ns == DEFAULT_NAMESPACE {
            note
        } else {
            format!("Tools whose names start with {ns} belong to the {ns} namespace:\n{note}")
        });
    }

    r.tool_choice = match v.get("tool_choice") {
        Some(Value::String(s)) => match s.as_str() {
            "auto" => Some(ToolChoice::Auto),
            "none" => Some(ToolChoice::None),
            "required" => Some(ToolChoice::Required),
            _ => None,
        },
        Some(o @ Value::Object(_)) => match str_of(o, "type") {
            Some("function" | "custom") => str_of(o, "name")
                .map(|n| ToolChoice::Named(flat_tool_name(str_of(o, "namespace"), n))),
            Some("allowed_tools") => {
                dropped.path("tool_choice.allowed_tools");
                match str_of(o, "mode") {
                    Some("required") => Some(ToolChoice::Required),
                    _ => Some(ToolChoice::Auto),
                }
            }
            other => {
                dropped.path(format!("tool_choice.{}", other.unwrap_or("unknown")));
                None
            }
        },
        _ => None,
    };

    if let Some(re) = v.get("reasoning").filter(|x| x.is_object()) {
        let effort = str_of(re, "effort").and_then(think::parse_openai);
        r.reasoning = Some(Reasoning {
            enabled: effort != Some(None),
            effort: effort.flatten(),
            budget: None,
            summary: has(re, "summary") || has(re, "generate_summary"),
        });
    }
    // 对话中途改过推理强度：以最后一次为准
    if let Some(e) = effort {
        match think::parse_openai(&e) {
            Some(effort) => {
                let re = r.reasoning.get_or_insert(Reasoning {
                    enabled: true,
                    effort: None,
                    budget: None,
                    summary: false,
                });
                re.enabled = effort.is_some();
                re.effort = effort;
            }
            // 模型自己定义的强度，别家没有对应
            None => dropped.path("input.configuration_update.reasoning.effort"),
        }
    }

    if let Some(text) = v.get("text") {
        r.format = match text.get("format").and_then(|f| str_of(f, "type")) {
            Some("json_object") => Some(Format::JsonObject),
            Some("json_schema") => {
                let f = &text["format"];
                Some(Format::JsonSchema {
                    name: str_of(f, "name").map(str::to_string),
                    schema: f.get("schema").cloned().unwrap_or(Value::Null),
                    strict: f.get("strict").and_then(Value::as_bool),
                })
            }
            _ => None,
        };
        if has(text, "verbosity") {
            r.verbosity = str_of(text, "verbosity").and_then(Verbosity::parse);
            if r.verbosity.is_none() {
                dropped.path("text.verbosity");
            }
        }
    }

    for k in [
        "top_logprobs",
        "max_tool_calls",
        "moderation",
        "context_management",
    ] {
        if has(v, k) {
            dropped.path(k);
        }
    }
    // `priority`、`flex` 是 OpenAI 的计费和排队档位，别家没有同样的东西
    if str_of(v, "service_tier").is_some_and(|t| !matches!(t, "auto" | "default")) {
        dropped.path("service_tier");
    }
    Ok(r)
}

/// namespace 里的工具展开成一个名字，写响应时再按 [`ClientShape::namespaced`] 拆回来。
///
/// - 默认 namespace（`functions`）不加前缀
/// - 别的照 Codex 自己拼名字的写法：分界处已经有 `_` 的直接接上
///   （`mcp__codex_apps__calendar` + `_create_event`），否则中间加 `__`（`mcp_fs__read`）
/// - **超过 64 个字符的截短，末尾换成哈希**：别家的工具名都限 64 个字符，namespace 再加
///   名字很容易超，超了整个请求被拒。哈希按 namespace 和名字算，同一个工具每次都是同一个名字
///
/// 工具定义和历史里的调用用的是同一个函数，所以对得上。会话记录读 Responses 请求时也用它，
/// 和转给别家时的名字一样
pub fn flat_tool_name(namespace: Option<&str>, name: &str) -> String {
    let ns = match namespace {
        Some(ns) if !ns.is_empty() && ns != DEFAULT_NAMESPACE => ns,
        _ => return name.to_string(),
    };
    let flat = if ns.ends_with('_') || name.starts_with('_') {
        format!("{ns}{name}")
    } else {
        format!("{ns}__{name}")
    };
    if flat.len() <= NAME_LIMIT {
        return flat;
    }
    let suffix = format!("_{:012x}", fnv1a(ns, name) & 0xffff_ffff_ffff);
    let mut cut = NAME_LIMIT - suffix.len();
    while !flat.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{suffix}", &flat[..cut])
}

/// FNV-1a，64 位。不用标准库的哈希：它不保证换个版本还是同一个值
fn fnv1a(ns: &str, name: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in ns.bytes().chain([0]).chain(name.bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 请求里定义成自由格式的工具，展开后的名字：顶层 `tools`、`additional_tools` 和
/// `tool_search_output` 里的都算。
///
/// 会话记录读上游的回答时要它：别家上游把自由格式工具的原文包在 `{"input": …}` 里，
/// 按这份名单拆回来
pub fn freeform_tools(v: &Value) -> Vec<String> {
    fn walk(t: &Value, namespace: Option<&str>, out: &mut Vec<String>) {
        match str_of(t, "type") {
            Some("custom") => out.push(flat_tool_name(
                namespace,
                str_of(t, "name").unwrap_or_default(),
            )),
            Some("namespace") if namespace.is_none() => {
                let ns = str_of(t, "name");
                for inner in arr_of(t, "tools") {
                    walk(inner, ns, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    let declared = arr_of(v, "input").iter().filter(|i| {
        matches!(
            str_of(i, "type"),
            Some("additional_tools" | "tool_search_output")
        )
    });
    for t in arr_of(v, "tools")
        .iter()
        .chain(declared.flat_map(|i| arr_of(i, "tools")))
    {
        walk(t, None, &mut out);
    }
    out
}

/// 解码出的工具。
///
/// **同名的后来者替换先前的，位置不变**：顶层 `tools` 在前，`input` 里的 `additional_tools`、
/// `tool_search_output` 按出现顺序在后。Codex 的增量声明就是这么说的（「重新定义的工具以
/// 最新的定义为准」）；位置不变，工具列表的开头就尽量稳定 —— 提示缓存从工具列表算起
#[derive(Default)]
struct ToolSet {
    tools: Vec<Tool>,
    at: HashMap<String, usize>,
    /// namespace 的说明，按第一次出现的顺序；后来的替换先前的
    notes: Vec<(String, String)>,
}

impl ToolSet {
    fn add(&mut self, t: Tool) {
        match self.at.get(&t.name) {
            Some(&i) => self.tools[i] = t,
            None => {
                self.at.insert(t.name.clone(), self.tools.len());
                self.tools.push(t);
            }
        }
    }

    fn note(&mut self, namespace: &str, note: &str) {
        match self.notes.iter_mut().find(|(ns, _)| ns == namespace) {
            Some((_, n)) => *n = note.to_string(),
            None => self.notes.push((namespace.to_string(), note.to_string())),
        }
    }
}

/// 解码时一路带着的东西
struct Ctx<'a> {
    dropped: &'a mut Dropped,
    shape: &'a mut ClientShape,
    tools: ToolSet,
    /// 最后一个 `configuration_update` 里的推理强度
    effort: Option<String>,
    /// 有 `compaction_trigger`：这一次要的是压缩
    compaction: bool,
}

impl Ctx<'_> {
    /// 一个工具定义。返回加进去的工具名，namespace 展开成里面的每一个。`at` 是它在客户端
    /// 请求里的位置，记丢弃用
    fn tool(&mut self, t: &Value, namespace: Option<&str>, at: &str) -> Vec<String> {
        let name = str_of(t, "name").unwrap_or_default();
        let (flat, kind) = match str_of(t, "type") {
            Some("function") => (flat_tool_name(namespace, name), function_kind(t)),
            Some("custom") => (
                flat_tool_name(namespace, name),
                ToolKind::Freeform {
                    format: t.get("format").cloned(),
                },
            ),
            Some("namespace") if namespace.is_none() => {
                // Codex 给没写说明的 namespace 填的是这句套话，不值得写进系统提示
                let filler = format!("Tools in the {name} namespace.");
                if let Some(note) = str_of(t, "description")
                    .map(str::trim)
                    .filter(|d| !d.is_empty() && *d != filler)
                {
                    self.tools.note(name, note);
                }
                return arr_of(t, "tools")
                    .iter()
                    .flat_map(|inner| self.tool(inner, Some(name), at))
                    .collect();
            }
            // 客户端自己执行的工具搜索：上游调用它时写回 `tool_search_call`，由客户端去搜
            Some("tool_search")
                if namespace.is_none() && str_of(t, "execution") == Some("client") =>
            {
                self.shape.tool_search = Some(TOOL_SEARCH.to_string());
                (TOOL_SEARCH.to_string(), function_kind(t))
            }
            // 托管工具（web_search、file_search、OpenAI 执行的 tool_search、shell、mcp……）
            // 只有 OpenAI 能执行
            other => {
                self.dropped
                    .path(format!("{at}.{}", other.unwrap_or("unknown")));
                return Vec::new();
            }
        };
        if let Some(ns) = namespace {
            self.shape
                .namespaced
                .insert(flat.clone(), (ns.to_string(), name.to_string()));
        }
        self.tools.add(Tool {
            name: flat.clone(),
            description: str_of(t, "description").map(str::to_string),
            kind,
        });
        vec![flat]
    }

    fn item(&mut self, item: &Value, r: &mut Request) -> Result<(), Rejection> {
        let kind = str_of(item, "type").unwrap_or("message");
        let push = |r: &mut Request, role, part| {
            r.messages.push(Message {
                role,
                parts: vec![part],
            })
        };
        match kind {
            "message" => {
                let content = item.get("content").unwrap_or(&Value::Null);
                match str_of(item, "role").unwrap_or("user") {
                    "system" | "developer" => {
                        let t = text_of(content);
                        if !t.is_empty() {
                            system_turn(r, t);
                        }
                    }
                    role => {
                        let role = if role == "assistant" {
                            Role::Assistant
                        } else {
                            Role::User
                        };
                        let parts = content_parts(content, "input.content", self.dropped);
                        r.messages.push(Message { role, parts });
                    }
                }
            }
            "additional_tools" => {
                for t in arr_of(item, "tools") {
                    self.tool(t, None, "input.additional_tools.tools");
                }
            }
            "agent_message" => {
                let mut parts = Vec::new();
                for p in arr_of(item, "content") {
                    match str_of(p, "type").unwrap_or("") {
                        "input_text" => {
                            if let Some(t) = str_of(p, "text").filter(|t| !t.is_empty()) {
                                parts.push(Part::Text(t.to_string()));
                            }
                        }
                        other => self
                            .dropped
                            .path(format!("input.agent_message.content.{other}")),
                    }
                }
                if !parts.is_empty() {
                    r.messages.push(Message {
                        role: Role::User,
                        parts,
                    });
                }
            }
            "function_call" | "custom_tool_call" => {
                let name = flat_tool_name(
                    str_of(item, "namespace"),
                    str_of(item, "name").unwrap_or_default(),
                );
                let input = if kind == "custom_tool_call" {
                    ToolInput::Text(str_of(item, "input").unwrap_or_default().to_string())
                } else {
                    ToolInput::from_json_text(str_of(item, "arguments").unwrap_or_default())
                };
                push(
                    r,
                    Role::Assistant,
                    Part::ToolCall(ToolCall {
                        id: str_of(item, "call_id").unwrap_or_default().to_string(),
                        name,
                        input,
                    }),
                );
            }
            // 结果是同一个 call_id 的 function_call_output。没有 call_id 的（更早的写法）
            // 配不上结果，留着反而是一个没有结果的调用
            "local_shell_call" => match str_of(item, "call_id") {
                Some(id) => push(
                    r,
                    Role::Assistant,
                    Part::ToolCall(ToolCall {
                        id: id.to_string(),
                        name: LOCAL_SHELL.to_string(),
                        input: ToolInput::Json(
                            item.get("action")
                                .filter(|a| a.is_object())
                                .cloned()
                                .unwrap_or_else(|| json!({})),
                        ),
                    }),
                ),
                None => self.dropped.path("input.local_shell_call"),
            },
            "function_call_output" | "custom_tool_call_output" => {
                let content = match item.get("output") {
                    Some(Value::String(s)) if !s.is_empty() => vec![Part::Text(s.clone())],
                    Some(o @ Value::Array(_)) => {
                        content_parts(o, &format!("input.{kind}.output"), self.dropped)
                            .into_iter()
                            .filter(|p| matches!(p, Part::Text(_) | Part::Image(_)))
                            .collect()
                    }
                    _ => Vec::new(),
                };
                push(
                    r,
                    Role::User,
                    Part::ToolResult(ToolResult {
                        id: str_of(item, "call_id").unwrap_or_default().to_string(),
                        content,
                        is_error: false,
                    }),
                );
            }
            "tool_search_call" => match str_of(item, "call_id") {
                Some(id) => push(
                    r,
                    Role::Assistant,
                    Part::ToolCall(ToolCall {
                        id: id.to_string(),
                        name: TOOL_SEARCH.to_string(),
                        input: match item.get("arguments") {
                            Some(Value::String(s)) => ToolInput::from_json_text(s),
                            Some(a @ Value::Object(_)) => ToolInput::Json(a.clone()),
                            _ => ToolInput::Json(json!({})),
                        },
                    }),
                ),
                None => self.dropped.path("input.tool_search_call"),
            },
            // 搜到的工具从此可以调用：加进工具列表。结果里写上它们在上游那边叫什么
            "tool_search_output" => {
                let names: Vec<String> = arr_of(item, "tools")
                    .iter()
                    .flat_map(|t| self.tool(t, None, "input.tool_search_output.tools"))
                    .collect();
                if let Some(id) = str_of(item, "call_id") {
                    let text = if names.is_empty() {
                        "No matching tools were found.".to_string()
                    } else {
                        format!("These tools are now available: {}", names.join(", "))
                    };
                    push(
                        r,
                        Role::User,
                        Part::ToolResult(ToolResult {
                            id: id.to_string(),
                            content: vec![Part::Text(text)],
                            is_error: false,
                        }),
                    );
                }
            }
            "configuration_update" => {
                match item.get("reasoning").and_then(|re| str_of(re, "effort")) {
                    Some(e) => self.effort = Some(e.to_string()),
                    None => self.dropped.path("input.configuration_update"),
                }
            }
            "reasoning" => {
                let texts = |key: &str| {
                    arr_of(item, key)
                        .iter()
                        .filter_map(|x| str_of(x, "text"))
                        .collect::<Vec<_>>()
                        .join("\n\n")
                };
                let text = match texts("content") {
                    t if t.is_empty() => texts("summary"),
                    t => t,
                };
                let signature = str_of(item, "encrypted_content")
                    .filter(|e| !e.is_empty())
                    .and_then(|enc| {
                        if enc.starts_with(CARRIED) {
                            Signature::read(enc, Vendor::OpenAi)
                        } else {
                            let id = str_of(item, "id").unwrap_or_default();
                            Some(Signature::new(Vendor::OpenAi, format!("{id}:{enc}")))
                        }
                    });
                push(
                    r,
                    Role::Assistant,
                    Part::Thinking(Thinking { text, signature }),
                );
            }
            "item_reference" => {
                return Err(Rejection(
                    "An item_reference in input points at something kept on OpenAI's servers, so the request cannot be converted for an upstream of another format."
                        .into(),
                ));
            }
            "compaction" | "context_compaction" => {
                match str_of(item, "encrypted_content").filter(|e| !e.is_empty()) {
                    // 转换写出去的：解回摘要，留在原位。摘要是上游写的、不是调用方说的话，
                    // 和对话中途的系统消息一样放（内容过滤、脱敏只看调用方的话）
                    Some(enc) if crate::compaction::is_carried(enc) => {
                        let summary = crate::compaction::read(enc).ok_or_else(|| {
                            Rejection(format!(
                                "A {kind} in input carries a summary written during an earlier conversion, but it is damaged, so the request cannot be converted for an upstream of another format."
                            ))
                        })?;
                        push(
                            r,
                            Role::System,
                            Part::Text(crate::compaction::restored(&summary)),
                        );
                    }
                    Some(_) => {
                        return Err(Rejection(format!(
                            "A {kind} in input is an encrypted, compacted conversation only OpenAI can read, so the request cannot be converted for an upstream of another format."
                        )));
                    }
                    // 没有内容的只是一个记号
                    None => self.dropped.path(format!("input.{kind}")),
                }
            }
            // 要压缩前文：历史照转，最后由 `decode_request` 加上写摘要的请求
            "compaction_trigger" => self.compaction = true,
            // web_search_call、image_generation_call……：服务端工具的执行记录
            other => self.dropped.path(format!("input.{other}")),
        }
        Ok(())
    }
}

/// 函数工具（还有客户端执行的工具搜索）的参数定义
fn function_kind(t: &Value) -> ToolKind {
    ToolKind::Function {
        schema: t
            .get("parameters")
            .filter(|p| !p.is_null())
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
        strict: t.get("strict").and_then(Value::as_bool),
    }
}

fn content_parts(content: &Value, prefix: &str, dropped: &mut Dropped) -> Vec<Part> {
    match content {
        Value::String(s) if !s.is_empty() => vec![Part::Text(s.clone())],
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match str_of(p, "type").unwrap_or("") {
                "input_text" | "output_text" => {
                    str_of(p, "text").map(|t| Part::Text(t.to_string()))
                }
                "refusal" => str_of(p, "refusal").map(|t| Part::Text(t.to_string())),
                "input_image" => match str_of(p, "image_url") {
                    Some(u) => Some(Part::Image(Media::from_uri(u))),
                    None => {
                        dropped.path(format!("{prefix}.input_image.file_id"));
                        None
                    }
                },
                "input_file" => {
                    let name = str_of(p, "filename").map(str::to_string);
                    if let Some(data) = str_of(p, "file_data") {
                        Some(Part::File {
                            media: match Media::from_uri(data) {
                                m @ Media::Base64 { .. } => m,
                                Media::Url(_) => Media::Base64 {
                                    mime: "application/pdf".into(),
                                    data: data.to_string(),
                                },
                            },
                            name,
                        })
                    } else if let Some(u) = str_of(p, "file_url") {
                        Some(Part::File {
                            media: Media::Url(u.to_string()),
                            name,
                        })
                    } else {
                        dropped.path(format!("{prefix}.input_file.file_id"));
                        None
                    }
                }
                other => {
                    dropped.path(format!("{prefix}.{other}"));
                    None
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

// ───────────────────────────────────────────────────────── 编码

/// 中间表示 → 发给 Responses 上游的请求。
pub fn encode_request(r: &Request, _t: &Target, dropped: &mut Dropped) -> Value {
    let freeform = |name: &str| {
        r.tools
            .iter()
            .any(|t| t.name == name && matches!(t.kind, ToolKind::Freeform { .. }))
    };
    let mut out = Map::new();
    out.insert("model".into(), json!(r.model));
    if !r.system.is_empty() {
        out.insert("instructions".into(), json!(r.system.join("\n\n")));
    }

    let mut input = Vec::new();
    for m in &r.messages {
        match m.role {
            // Responses 有对话中途的 developer 消息，原样放在原位
            Role::System => {
                let content: Vec<Value> = m
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::Text(t) if !t.is_empty() => {
                            Some(json!({ "type": "input_text", "text": t }))
                        }
                        _ => None,
                    })
                    .collect();
                if !content.is_empty() {
                    input.push(
                        json!({ "type": "message", "role": "developer", "content": content }),
                    );
                }
            }
            Role::User => {
                let mut content = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::ToolResult(res) => {
                            let kind = if freeform_result(r, &res.id) {
                                "custom_tool_call_output"
                            } else {
                                "function_call_output"
                            };
                            input.push(json!({
                                "type": kind,
                                "call_id": res.id,
                                "output": result_output(res),
                            }));
                        }
                        Part::Text(t) if !t.is_empty() => {
                            content.push(json!({ "type": "input_text", "text": t }))
                        }
                        Part::Image(media) => content.push(json!({
                            "type": "input_image",
                            "image_url": media.to_uri(),
                            "detail": "auto",
                        })),
                        Part::File { media, name } => content.push(match media {
                            Media::Base64 { .. } => json!({
                                "type": "input_file",
                                "file_data": media.to_uri(),
                                "filename": name.as_deref().unwrap_or("file.pdf"),
                            }),
                            Media::Url(u) => json!({ "type": "input_file", "file_url": u }),
                        }),
                        _ => {}
                    }
                }
                if !content.is_empty() {
                    input.push(json!({ "type": "message", "role": "user", "content": content }));
                }
            }
            Role::Assistant => {
                let mut text = String::new();
                let flush = |text: &mut String, input: &mut Vec<Value>| {
                    if !text.is_empty() {
                        input.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": std::mem::take(text),
                        }));
                    }
                };
                for p in &m.parts {
                    match p {
                        Part::Text(t) => text.push_str(t),
                        Part::Thinking(th) => match th.signature.as_ref() {
                            Some(s) if s.vendor == Vendor::OpenAi => {
                                flush(&mut text, &mut input);
                                let (id, enc) = s.value.split_once(':').unwrap_or(("", &s.value));
                                let summary: Vec<Value> = if th.text.is_empty() {
                                    Vec::new()
                                } else {
                                    vec![json!({ "type": "summary_text", "text": th.text })]
                                };
                                input.push(json!({
                                    "type": "reasoning",
                                    "id": id,
                                    "summary": summary,
                                    "encrypted_content": enc,
                                }));
                            }
                            _ => dropped.feature(Feature::ReasoningHistory),
                        },
                        Part::ToolCall(c) => {
                            flush(&mut text, &mut input);
                            input.push(match &c.input {
                                ToolInput::Text(t) if freeform(&c.name) => json!({
                                    "type": "custom_tool_call",
                                    "call_id": c.id,
                                    "name": c.name,
                                    "input": t,
                                }),
                                other => json!({
                                    "type": "function_call",
                                    "call_id": c.id,
                                    "name": c.name,
                                    "arguments": other.to_json_text(),
                                }),
                            });
                        }
                        _ => {}
                    }
                }
                flush(&mut text, &mut input);
            }
        }
    }
    out.insert("input".into(), Value::Array(input));

    if !r.tools.is_empty() {
        let tools = r
            .tools
            .iter()
            .map(|tool| {
                let mut o = match &tool.kind {
                    // **strict 要明确写 false**：Responses 默认按严格模式校验 schema，
                    // 别家客户端的工具定义几乎都过不了
                    ToolKind::Function { schema, strict } => json!({
                        "type": "function",
                        "name": tool.name,
                        "parameters": schema,
                        "strict": strict.unwrap_or(false),
                    }),
                    ToolKind::Freeform { format } => {
                        let mut o = json!({ "type": "custom", "name": tool.name });
                        if let Some(f) = format {
                            o["format"] = f.clone();
                        }
                        o
                    }
                };
                if let Some(d) = &tool.description {
                    o["description"] = json!(d);
                }
                o
            })
            .collect();
        out.insert("tools".into(), Value::Array(tools));
        if let Some(c) = &r.tool_choice {
            out.insert(
                "tool_choice".into(),
                match c {
                    ToolChoice::Auto => json!("auto"),
                    ToolChoice::None => json!("none"),
                    ToolChoice::Required => json!("required"),
                    ToolChoice::Named(n) if freeform(n) => json!({ "type": "custom", "name": n }),
                    ToolChoice::Named(n) => json!({ "type": "function", "name": n }),
                },
            );
        }
        if let Some(p) = r.parallel_tool_calls {
            out.insert("parallel_tool_calls".into(), json!(p));
        }
    }

    if let Some(n) = r.max_tokens {
        out.insert("max_output_tokens".into(), json!(n));
    }
    if let Some(x) = r.temperature {
        out.insert("temperature".into(), json!(x));
    }
    if let Some(x) = r.top_p {
        out.insert("top_p".into(), json!(x));
    }
    for (present, f) in [
        (r.top_k.is_some(), Feature::TopK),
        (!r.stop.is_empty(), Feature::Stop),
        (r.seed.is_some(), Feature::Seed),
        (r.presence_penalty.is_some(), Feature::PresencePenalty),
        (r.frequency_penalty.is_some(), Feature::FrequencyPenalty),
    ] {
        if present {
            dropped.feature(f);
        }
    }

    match &r.reasoning {
        Some(re) if re.enabled => {
            let mut o = Map::new();
            if let Some(e) = think::effort(re) {
                o.insert("effort".into(), json!(think::openai(e)));
            }
            if re.summary {
                o.insert("summary".into(), json!("auto"));
            }
            if !o.is_empty() {
                out.insert("reasoning".into(), Value::Object(o));
            }
            // 推理内容加密带回来，下一轮才能接上
            out.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }
        Some(_) => dropped.feature(Feature::Reasoning),
        None => {}
    }

    let mut text = Map::new();
    match &r.format {
        Some(Format::JsonObject) => {
            text.insert("format".into(), json!({ "type": "json_object" }));
        }
        Some(Format::JsonSchema {
            name,
            schema,
            strict,
        }) => {
            let mut f = json!({
                "type": "json_schema",
                "name": name.as_deref().unwrap_or("output"),
                "schema": schema,
            });
            if let Some(s) = strict {
                f["strict"] = json!(s);
            }
            text.insert("format".into(), f);
        }
        None => {}
    }
    match r.verbosity {
        Some(x) if Verbosity::understood_by(&r.model) => {
            text.insert("verbosity".into(), json!(x.as_str()));
        }
        Some(_) => dropped.feature(Feature::Verbosity),
        None => {}
    }
    if !text.is_empty() {
        out.insert("text".into(), Value::Object(text));
    }

    // 不让 OpenAI 保存这次对话：转换过来的请求本来就带着完整的上下文
    out.insert("store".into(), json!(false));
    if r.stream {
        out.insert("stream".into(), json!(true));
    }
    Value::Object(out)
}

/// 这个工具结果对应的调用是自由格式工具发起的
fn freeform_result(r: &Request, call_id: &str) -> bool {
    r.messages.iter().flat_map(|m| &m.parts).any(|p| {
        matches!(p, Part::ToolCall(c) if c.id == call_id && matches!(c.input, ToolInput::Text(_))
            && r.tools.iter().any(|t| t.name == c.name && matches!(t.kind, ToolKind::Freeform { .. })))
    })
}

fn result_output(res: &ToolResult) -> Value {
    if !res.has_image() {
        return json!(res.text());
    }
    Value::Array(
        res.content
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(json!({ "type": "input_text", "text": t })),
                Part::Image(m) => Some(
                    json!({ "type": "input_image", "image_url": m.to_uri(), "detail": "auto" }),
                ),
                _ => None,
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(s: &str) -> Result<(Request, Vec<String>, ClientShape), Rejection> {
        let mut d = Dropped::new(Dialect::Responses);
        let mut shape = ClientShape::default();
        let r = decode_request(&serde_json::from_str(s).unwrap(), &mut d, &mut shape)?;
        Ok((r, d.into_vec(), shape))
    }

    fn encode(r: &Request, client: Dialect) -> (Value, Vec<String>) {
        let mut d = Dropped::new(client);
        let t = Target {
            dialect: Dialect::Responses,
            official: true,
            default_max_tokens: 8192,
        };
        (encode_request(r, &t, &mut d), d.into_vec())
    }

    /// Codex CLI 发的那种请求：instructions、developer 消息、函数工具和自由格式的
    /// apply_patch、加密的推理项、工具调用和结果。
    const CODEX: &str = r#"{
        "model": "gpt-5.1-codex",
        "instructions": "You are Codex.",
        "input": [
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "sandbox: workspace-write"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "修一下 bug"}]},
            {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "先看代码"}], "encrypted_content": "gAAAAB"},
            {"type": "function_call", "call_id": "call_1", "name": "shell", "arguments": "{\"command\":[\"ls\"]}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "src"},
            {"type": "custom_tool_call", "call_id": "call_2", "name": "apply_patch", "input": "*** Begin Patch"},
            {"type": "custom_tool_call_output", "call_id": "call_2", "output": "Done"},
            {"type": "web_search_call", "id": "ws_1", "status": "completed"}
        ],
        "tools": [
            {"type": "function", "name": "shell", "parameters": {"type": "object"}, "strict": false},
            {"type": "custom", "name": "apply_patch", "format": {"type": "grammar", "syntax": "lark", "definition": "start: x"}},
            {"type": "namespace", "name": "mcp_fs", "description": "files", "tools": [
                {"type": "function", "name": "read", "parameters": {"type": "object"}}
            ]},
            {"type": "web_search"}
        ],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "reasoning": {"effort": "high", "summary": "auto"},
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "k"
    }"#;

    #[test]
    fn a_codex_request_decodes_into_turns_tools_and_reasoning() {
        let (r, dropped, shape) = decode(CODEX).unwrap();
        // namespace 的说明进系统提示
        assert_eq!(
            r.system,
            [
                "You are Codex.",
                "sandbox: workspace-write",
                "Tools whose names start with mcp_fs belong to the mcp_fs namespace:\nfiles"
            ]
        );
        let Part::Thinking(th) = &r.messages[1].parts[0] else {
            panic!("{:?}", r.messages[1]);
        };
        assert_eq!(th.text, "先看代码");
        assert_eq!(
            th.signature,
            Some(Signature::new(Vendor::OpenAi, "rs_1:gAAAAB"))
        );
        assert!(
            matches!(&r.messages[4].parts[0], Part::ToolCall(c) if c.input == ToolInput::Text("*** Begin Patch".into()))
        );
        let names: Vec<&str> = r.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["shell", "apply_patch", "mcp_fs__read"]);
        assert_eq!(
            shape.namespaced.get("mcp_fs__read"),
            Some(&("mcp_fs".to_string(), "read".to_string()))
        );
        let re = r.reasoning.unwrap();
        assert_eq!(re.effort, Some(Effort::High));
        assert!(re.summary);
        assert_eq!(dropped, ["tools.web_search", "input.web_search_call"]);
    }

    #[test]
    fn a_request_that_depends_on_server_state_is_refused() {
        for body in [
            r#"{"model":"m","previous_response_id":"resp_1","input":"hi"}"#,
            r#"{"model":"m","input":[{"type":"item_reference","id":"msg_1"}]}"#,
            r#"{"model":"m","input":[{"type":"compaction","encrypted_content":"x"}]}"#,
            r#"{"model":"m","input":[{"type":"context_compaction","encrypted_content":"x"}]}"#,
            // 前缀是转换写出去的，内容坏了
            r#"{"model":"m","input":[{"type":"compaction","encrypted_content":"tw1.c.a"}]}"#,
            r#"{"model":"m","background":true,"input":"hi"}"#,
        ] {
            let e = decode(body).unwrap_err();
            assert!(e.0.contains("cannot be converted"), "{body}: {e}");
        }
        // null 不算用了
        assert!(decode(r#"{"model":"m","previous_response_id":null,"input":"hi"}"#).is_ok());
        // 没有密文的 context_compaction 只是一个记号
        let (_, dropped, _) =
            decode(r#"{"model":"m","input":[{"type":"context_compaction"},{"role":"user","content":"hi"}]}"#)
                .unwrap();
        assert_eq!(dropped, ["input.context_compaction"]);
    }

    #[test]
    fn a_compaction_trigger_asks_the_upstream_for_a_summary_at_the_end() {
        let (r, dropped, shape) = decode(
            r#"{"model": "m", "input": [
                {"type": "message", "role": "developer", "content": "You are Codex."},
                {"type": "message", "role": "user", "content": "fix it"},
                {"type": "message", "role": "assistant", "content": "done"},
                {"type": "compaction_trigger"}
            ]}"#,
        )
        .unwrap();
        assert!(shape.compaction);
        assert!(dropped.is_empty(), "{dropped:?}");
        assert_eq!(r.system, ["You are Codex."]);
        let last = r.messages.last().unwrap();
        assert_eq!(last.role, Role::User);
        assert_eq!(
            last.parts,
            [Part::Text(crate::compaction::INSTRUCTION.into())]
        );
        // 没有它就是普通的一轮
        let (r, _, shape) =
            decode(r#"{"model": "m", "input": [{"role": "user", "content": "hi"}]}"#).unwrap();
        assert!(!shape.compaction);
        assert_eq!(r.messages.len(), 1);
    }

    #[test]
    fn a_summary_written_during_conversion_comes_back_in_its_place() {
        let body = json!({"model": "m", "input": [
            {"type": "message", "role": "developer", "content": "You are Codex."},
            {"type": "message", "role": "user", "content": "fix it"},
            {"type": "compaction", "encrypted_content": crate::compaction::carry("Edited src/a.rs; tests pass.")},
            {"type": "message", "role": "developer", "content": "<permissions instructions>"},
            {"type": "message", "role": "user", "content": "now the docs"}
        ]})
        .to_string();
        let (r, dropped, _) = decode(&body).unwrap();
        assert!(dropped.is_empty(), "{dropped:?}");
        assert_eq!(r.system, ["You are Codex."]);
        let roles: Vec<Role> = r.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, [Role::User, Role::System, Role::System, Role::User]);
        assert_eq!(
            r.messages[1].parts,
            [Part::Text(crate::compaction::restored(
                "Edited src/a.rs; tests pass."
            ))]
        );
        assert_eq!(
            r.messages[2].parts,
            [Part::Text("<permissions instructions>".into())]
        );
    }

    /// Codex 的 Responses Lite（`use_responses_lite`）：顶层没有 `tools`，所有工具装在
    /// `input` 开头的 `additional_tools` 里，函数和自由格式工具在 `functions` 这个
    /// namespace 里（`codex-rs/core/src/client.rs`、`codex-rs/tools/src/tool_spec.rs`）
    const LITE: &str = r#"{
        "model": "gpt-5.4",
        "stream": true,
        "input": [
            {"id": "at_1", "type": "additional_tools", "role": "developer", "tools": [
                {"type": "namespace", "name": "functions", "description": "", "tools": [
                    {"type": "function", "name": "exec_command", "description": "Runs a command.", "strict": false,
                     "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"], "additionalProperties": false}},
                    {"type": "custom", "name": "apply_patch", "description": "Edit files.",
                     "format": {"type": "grammar", "syntax": "lark", "definition": "start: begin_patch hunk+ end_patch"}}
                ]},
                {"type": "namespace", "name": "mcp__codex_apps__calendar", "description": "Plan events.", "tools": [
                    {"type": "function", "name": "_create_event", "description": "Create a calendar event.", "strict": false,
                     "parameters": {"type": "object", "properties": {"title": {"type": "string"}}}}
                ]},
                {"type": "namespace", "name": "web", "description": "Tools in the web namespace.", "tools": [
                    {"type": "function", "name": "run", "description": "Search the web.", "strict": false, "parameters": {"type": "object"}}
                ]},
                {"type": "tool_search", "execution": "client", "description": "Search deferred tools.",
                 "parameters": {"type": "object", "properties": {"query": {"type": "string"}, "limit": {"type": "number"}}, "required": ["query"]}}
            ]},
            {"id": "msg_1", "type": "message", "role": "developer", "content": [{"type": "input_text", "text": "You are Codex."}],
             "internal_chat_message_metadata_passthrough": {"content_item_kinds": ["model.base_instructions"]}},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "fix the test"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Looking."}], "phase": "commentary"},
            {"type": "function_call", "name": "exec_command", "namespace": "functions", "arguments": "{\"cmd\":\"ls\"}", "call_id": "call_1"},
            {"type": "function_call_output", "call_id": "call_1", "output": "src"},
            {"type": "local_shell_call", "id": "lsh_1", "call_id": "call_2", "status": "completed",
             "action": {"type": "exec", "command": ["echo", "hi"], "timeout_ms": null, "working_directory": null, "env": null, "user": null}},
            {"type": "function_call_output", "call_id": "call_2", "output": "hi"},
            {"type": "tool_search_call", "call_id": "call_3", "execution": "client", "status": "completed", "arguments": {"query": "drive", "limit": 8}},
            {"type": "tool_search_output", "call_id": "call_3", "status": "completed", "execution": "client", "tools": [
                {"type": "namespace", "name": "mcp__codex_apps__drive", "description": "Files in Drive.", "tools": [
                    {"type": "function", "name": "_search", "description": "Search files.", "strict": false, "defer_loading": true, "parameters": {"type": "object"}}
                ]}
            ]},
            {"type": "configuration_update", "reasoning": {"effort": "high"}},
            {"type": "agent_message", "author": "/root", "recipient": "/root/worker", "content": [
                {"type": "input_text", "text": "Message Type: MESSAGE\nPayload:\nrun it"},
                {"type": "encrypted_content", "encrypted_content": "gAAA"}
            ]},
            {"type": "web_search_call", "id": "ws_1", "status": "completed", "action": {"type": "search", "query": "x"}}
        ],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "reasoning": {"effort": "medium", "summary": "auto", "context": "all_turns"},
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "019a",
        "text": {"verbosity": "low"},
        "client_metadata": {"x-codex-turn-metadata": "{}"}
    }"#;

    #[test]
    fn a_responses_lite_request_keeps_the_tools_it_declares_in_input() {
        let (r, dropped, shape) = decode(LITE).unwrap();
        let names: Vec<&str> = r.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "exec_command",
                "apply_patch",
                "mcp__codex_apps__calendar_create_event",
                "web__run",
                "tool_search",
                // tool_search 搜到的，从此可以调用
                "mcp__codex_apps__drive_search",
            ]
        );
        assert!(matches!(r.tools[1].kind, ToolKind::Freeform { .. }));
        assert_eq!(
            shape.namespaced.get("exec_command"),
            Some(&("functions".to_string(), "exec_command".to_string()))
        );
        assert_eq!(
            shape
                .namespaced
                .get("mcp__codex_apps__calendar_create_event"),
            Some(&(
                "mcp__codex_apps__calendar".to_string(),
                "_create_event".to_string()
            ))
        );
        assert_eq!(shape.tool_search.as_deref(), Some("tool_search"));
        // MCP 服务器的说明进系统提示；Codex 自己填的套话不进
        assert_eq!(
            r.system,
            [
                "You are Codex.",
                "Tools whose names start with mcp__codex_apps__calendar belong to the mcp__codex_apps__calendar namespace:\nPlan events.",
                "Tools whose names start with mcp__codex_apps__drive belong to the mcp__codex_apps__drive namespace:\nFiles in Drive."
            ]
        );
        // 对话中途改成了 high：盖过顶层开头那一档
        assert_eq!(r.reasoning.as_ref().unwrap().effort, Some(Effort::High));
        assert_eq!(r.verbosity, Some(Verbosity::Low));
        assert_eq!(
            dropped,
            [
                "input.agent_message.content.encrypted_content",
                "input.web_search_call"
            ]
        );

        let parts: Vec<&Part> = r.messages.iter().flat_map(|m| &m.parts).collect();
        // 历史里默认 namespace 的调用和工具同名
        assert!(
            matches!(parts[2], Part::ToolCall(c) if c.name == "exec_command" && c.input == ToolInput::Json(json!({"cmd": "ls"})))
        );
        // local_shell_call 和它的结果成对留着
        let Part::ToolCall(shell) = parts[4] else {
            panic!("{:?}", parts[4]);
        };
        assert_eq!(
            (shell.id.as_str(), shell.name.as_str()),
            ("call_2", "local_shell")
        );
        let ToolInput::Json(action) = &shell.input else {
            panic!("{shell:?}");
        };
        assert_eq!(action["command"], json!(["echo", "hi"]));
        assert!(
            matches!(parts[5], Part::ToolResult(res) if res.id == "call_2" && res.text() == "hi")
        );
        // 工具搜索的调用和结果
        assert!(
            matches!(parts[6], Part::ToolCall(c) if c.name == "tool_search" && c.input == ToolInput::Json(json!({"query": "drive", "limit": 8})))
        );
        assert!(
            matches!(parts[7], Part::ToolResult(res) if res.id == "call_3" && res.text() == "These tools are now available: mcp__codex_apps__drive_search")
        );
        // 别的代理发来的话是用户的一轮
        let last = r.messages.last().unwrap();
        assert_eq!(last.role, Role::User);
        assert_eq!(
            last.parts,
            [Part::Text("Message Type: MESSAGE\nPayload:\nrun it".into())]
        );
    }

    #[test]
    fn a_later_declaration_replaces_an_earlier_one_in_place() {
        // 顶层 tools 和 additional_tools 一起来，还有 Codex 的增量声明：同名的以最后一次为准，
        // 位置是第一次出现的位置
        let (r, dropped, _) = decode(
            r#"{"model": "m",
                "tools": [
                    {"type": "function", "name": "shell", "description": "old", "parameters": {"type": "object"}},
                    {"type": "function", "name": "plan", "parameters": {"type": "object"}}
                ],
                "input": [
                    {"type": "additional_tools", "role": "developer", "tools": [
                        {"type": "namespace", "name": "functions", "tools": [
                            {"type": "function", "name": "shell", "description": "new", "parameters": {"type": "object"}},
                            {"type": "custom", "name": "apply_patch"}
                        ]},
                        {"type": "web_search"}
                    ]},
                    {"role": "user", "content": "hi"},
                    {"type": "additional_tools", "role": "developer", "tools": [
                        {"type": "namespace", "name": "functions", "tools": [
                            {"type": "custom", "name": "plan", "description": "now freeform"}
                        ]}
                    ]}
                ]}"#,
        )
        .unwrap();
        let tools: Vec<(&str, Option<&str>, bool)> = r
            .tools
            .iter()
            .map(|t| {
                (
                    t.name.as_str(),
                    t.description.as_deref(),
                    matches!(t.kind, ToolKind::Freeform { .. }),
                )
            })
            .collect();
        assert_eq!(
            tools,
            [
                ("shell", Some("new"), false),
                ("plan", Some("now freeform"), true),
                ("apply_patch", None, true),
            ]
        );
        assert_eq!(dropped, ["input.additional_tools.tools.web_search"]);
    }

    #[test]
    fn a_long_namespaced_name_is_cut_to_64_with_a_stable_hash() {
        assert_eq!(flat_tool_name(None, "shell"), "shell");
        assert_eq!(flat_tool_name(Some("functions"), "shell"), "shell");
        assert_eq!(flat_tool_name(Some("mcp_fs"), "read"), "mcp_fs__read");
        assert_eq!(
            flat_tool_name(Some("mcp__codex_apps__calendar"), "_create_event"),
            "mcp__codex_apps__calendar_create_event"
        );
        let ns = "mcp__a_rather_long_server_name_from_some_connector";
        let a = flat_tool_name(Some(ns), "create_a_very_descriptive_thing");
        assert_eq!(a.len(), 64);
        assert!(a.starts_with(ns), "{a}");
        // 同一个工具每次同一个名字，不同的工具不撞
        assert_eq!(
            a,
            flat_tool_name(Some(ns), "create_a_very_descriptive_thing")
        );
        assert_ne!(
            a,
            flat_tool_name(Some(ns), "create_a_very_descriptive_thinG")
        );
    }

    #[test]
    fn freeform_tools_are_found_wherever_they_are_declared() {
        let v: Value = serde_json::from_str(LITE).unwrap();
        assert_eq!(freeform_tools(&v), ["apply_patch"]);
    }

    #[test]
    fn verbosity_goes_only_to_models_that_understand_it() {
        let (mut r, _, _) = decode(LITE).unwrap();
        let (v, dropped) = encode(&r, Dialect::Responses);
        assert_eq!(v["text"], json!({"verbosity": "low"}));
        assert!(!dropped.contains(&"text.verbosity".to_string()));
        r.model = "gpt-4.1".into();
        let (v, dropped) = encode(&r, Dialect::Responses);
        assert!(v.get("text").is_none());
        assert!(dropped.contains(&"text.verbosity".to_string()));
        for (model, ok) in [
            ("gpt-5", true),
            ("gpt-5.4-mini", true),
            ("openai/gpt-6-luna", true),
            ("gpt-4o", false),
            ("gpt-oss-120b", false),
            ("claude-opus-4-7", false),
        ] {
            assert_eq!(Verbosity::understood_by(model), ok, "{model}");
        }
    }

    #[test]
    fn back_to_responses_the_items_keep_their_kinds() {
        let (r, _, _) = decode(CODEX).unwrap();
        let (v, dropped) = encode(&r, Dialect::Responses);
        assert!(dropped.is_empty(), "{dropped:?}");
        let items = v["input"].as_array().unwrap();
        let kinds: Vec<&str> = items.iter().map(|i| i["type"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            [
                "message",
                "reasoning",
                "function_call",
                "function_call_output",
                "custom_tool_call",
                "custom_tool_call_output"
            ]
        );
        assert_eq!(items[1]["id"], "rs_1");
        assert_eq!(items[1]["encrypted_content"], "gAAAAB");
        assert_eq!(
            v["instructions"],
            "You are Codex.\n\nsandbox: workspace-write\n\nTools whose names start with mcp_fs belong to the mcp_fs namespace:\nfiles"
        );
        assert_eq!(v["tools"][0]["strict"], false);
        assert_eq!(v["tools"][1]["type"], "custom");
        assert_eq!(v["reasoning"], json!({"effort": "high", "summary": "auto"}));
        assert_eq!(v["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(v["store"], false);
    }

    #[test]
    fn a_claude_conversation_goes_out_as_items_and_foreign_thinking_is_reported() {
        let r = Request {
            model: "gpt-5".into(),
            system: vec!["sys".into()],
            max_tokens: Some(1000),
            stop: vec!["END".into()],
            top_k: Some(5),
            messages: vec![
                Message {
                    role: Role::User,
                    parts: vec![Part::Text("hi".into())],
                },
                Message {
                    role: Role::Assistant,
                    parts: vec![
                        Part::Thinking(Thinking {
                            text: "t".into(),
                            signature: Some(Signature::new(Vendor::Anthropic, "sig")),
                        }),
                        Part::Text("我看看".into()),
                        Part::ToolCall(ToolCall {
                            id: "toolu_1".into(),
                            name: "Read".into(),
                            input: ToolInput::Json(json!({"p": 1})),
                        }),
                    ],
                },
                Message {
                    role: Role::User,
                    parts: vec![Part::ToolResult(ToolResult {
                        id: "toolu_1".into(),
                        content: vec![Part::Text("内容".into())],
                        is_error: false,
                    })],
                },
            ],
            tools: vec![Tool {
                name: "Read".into(),
                description: Some("读".into()),
                kind: ToolKind::Function {
                    schema: json!({"type": "object"}),
                    strict: None,
                },
            }],
            reasoning: Some(Reasoning {
                enabled: true,
                effort: None,
                budget: Some(10000),
                summary: true,
            }),
            ..Default::default()
        };
        let (v, dropped) = encode(&r, Dialect::Anthropic);
        let items = v["input"].as_array().unwrap();
        assert_eq!(
            items[1],
            json!({"type": "message", "role": "assistant", "content": "我看看"})
        );
        assert_eq!(items[2]["arguments"], "{\"p\":1}");
        assert_eq!(
            items[3],
            json!({"type": "function_call_output", "call_id": "toolu_1", "output": "内容"})
        );
        assert_eq!(v["max_output_tokens"], 1000);
        assert_eq!(v["reasoning"]["effort"], "medium");
        assert_eq!(
            dropped,
            ["messages.content.thinking", "top_k", "stop_sequences"]
        );
    }
}
