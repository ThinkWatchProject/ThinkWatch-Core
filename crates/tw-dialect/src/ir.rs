//! 四种格式之间的中间表示。
//!
//! # 为什么经过中间表示
//!
//! 四种格式两两互转是 12 个方向，每个方向又分请求、整包响应、流式响应三件事。
//! 两两直接写是 36 份转换；经过中间表示，每种格式只写「解码到它」和「从它编码」，
//! 四种格式共 24 份，再加一种格式只多 6 份。
//!
//! # 装什么、不装什么
//!
//! **只装四种格式里能互相对应的部分。**只有一种格式有、别处没有对应物的东西
//! （Anthropic 的服务端工具、Responses 的托管工具、Gemini 的安全设置），解码时就记进
//! [`Dropped`]，不进中间表示。中间表示里有、目标格式表达不了的（Responses 没有
//! `stop`），编码时记。两处记的都是**客户端请求里的字段路径**：用户对照的是自己
//! 发出去的请求，不是我们转成的那一份。

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

/// 四种接口格式。客户端和上游都用它描述。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dialect {
    /// Anthropic Messages：`/v1/messages`
    Anthropic,
    /// OpenAI Chat Completions：`/v1/chat/completions`
    Chat,
    /// OpenAI Responses：`/v1/responses`
    Responses,
    /// Gemini：`/v1beta/models/{model}:generateContent`
    Gemini,
    /// Bedrock Converse：`/model/{id}/converse`
    Bedrock,
}

impl Dialect {
    /// 和配置里协议的写法一致
    pub fn slug(self) -> &'static str {
        match self {
            Dialect::Anthropic => "anthropic",
            Dialect::Chat => "openai-chat",
            Dialect::Responses => "openai-responses",
            Dialect::Gemini => "gemini",
            Dialect::Bedrock => "bedrock",
        }
    }

    /// 这种格式的推理签名原本属于哪家
    pub fn vendor(self) -> Vendor {
        match self {
            Dialect::Anthropic => Vendor::Anthropic,
            Dialect::Chat | Dialect::Responses => Vendor::OpenAi,
            Dialect::Gemini => Vendor::Google,
            // Converse 上跑的是各家原厂模型。推理签名按 Anthropic 认 ——
            // Bedrock 上有推理的就是 Claude
            Dialect::Bedrock => Vendor::Anthropic,
        }
    }
}

// ───────────────────────────────────────────────────────── 请求

/// 一次生成请求。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Request {
    pub model: String,
    /// 系统提示，按出现顺序
    pub system: Vec<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
    pub tool_choice: Option<ToolChoice>,
    /// `Some(false)` 表示一次只调一个工具
    pub parallel_tool_calls: Option<bool>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u64>,
    pub stop: Vec<String>,
    pub seed: Option<i64>,
    pub presence_penalty: Option<f64>,
    pub frequency_penalty: Option<f64>,
    pub reasoning: Option<Reasoning>,
    pub format: Option<Format>,
    pub stream: bool,
}

/// 转换时要知道的上游情况。
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub dialect: Dialect,
    /// 厂商官方端点。官方端点对参数更严格：Anthropic 官方已不接受 `temperature`
    /// 以外的采样参数，OpenAI 官方的推理模型只认 `max_completion_tokens`
    pub official: bool,
    /// 客户端没写最大输出、目标又必须写（Anthropic）时用多少
    pub default_max_tokens: u64,
}

/// 解码客户端请求时顺带记下的、写响应时要用的东西。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClientShape {
    /// Responses 客户端的 namespace 工具：展开后的名字 → (namespace, 名字)
    pub namespaced: std::collections::HashMap<String, (String, String)>,
    /// Chat 客户端要流末尾的用量块（`stream_options.include_usage`）
    pub include_usage: bool,
    /// Gemini 客户端要 SSE（`alt=sse`）；否则流是一个逐步写出的 JSON 数组
    pub gemini_sse: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(String),
    Image(Media),
    /// PDF 之类的文件
    File {
        media: Media,
        name: Option<String>,
    },
    Thinking(Thinking),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Media {
    Base64 { mime: String, data: String },
    Url(String),
}

impl Media {
    /// `data:` URI 拆成两段；别的当 URL
    pub fn from_uri(uri: &str) -> Media {
        if let Some(rest) = uri.strip_prefix("data:")
            && let Some((head, data)) = rest.split_once(',')
            && let Some(mime) = head.strip_suffix(";base64")
        {
            return Media::Base64 {
                mime: mime.to_string(),
                data: data.to_string(),
            };
        }
        Media::Url(uri.to_string())
    }

    /// 写成一个 URI：内联数据是 `data:` URI
    pub fn to_uri(&self) -> String {
        match self {
            Media::Base64 { mime, data } => format!("data:{mime};base64,{data}"),
            Media::Url(u) => u.clone(),
        }
    }
}

/// 一段推理内容。
#[derive(Debug, Clone, PartialEq)]
pub struct Thinking {
    pub text: String,
    pub signature: Option<Signature>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: ToolInput,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolInput {
    /// 函数工具的参数
    Json(Value),
    /// 自由格式工具的原文（Responses 的 custom tool，Codex 的 `apply_patch` 就是这种）
    Text(String),
}

impl ToolInput {
    /// 写成 JSON 文本。自由格式的原文包成 `{"input": 原文}` —— 只有 JSON 工具的格式
    /// 就用这个形状承载它，工具定义那边也按这个形状声明（见 [`freeform_schema`]）
    pub fn to_json_text(&self) -> String {
        match self {
            ToolInput::Json(v) => v.to_string(),
            ToolInput::Text(s) => serde_json::json!({ "input": s }).to_string(),
        }
    }

    /// 写成 JSON 对象。参数不是对象时包一层，Anthropic 和 Gemini 只收对象
    pub fn to_object(&self) -> Value {
        match self {
            ToolInput::Json(v @ Value::Object(_)) => v.clone(),
            ToolInput::Json(v) => serde_json::json!({ "value": v }),
            ToolInput::Text(s) => serde_json::json!({ "input": s }),
        }
    }

    /// 从 JSON 文本读回。解不开时原文当字符串保留：丢掉参数比一个形状奇怪的参数糟得多
    pub fn from_json_text(s: &str) -> ToolInput {
        if s.trim().is_empty() {
            return ToolInput::Json(Value::Object(Default::default()));
        }
        ToolInput::Json(serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_string())))
    }
}

/// 自由格式工具在只有 JSON 工具的格式里怎么声明。
pub fn freeform_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": { "input": { "type": "string" } },
        "required": ["input"],
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub id: String,
    /// 只会是 `Text` 或 `Image`
    pub content: Vec<Part>,
    pub is_error: bool,
}

impl ToolResult {
    /// 结果里的文字，按出现顺序连起来
    pub fn text(&self) -> String {
        let texts: Vec<&str> = self
            .content
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        texts.join("\n")
    }

    pub fn has_image(&self) -> bool {
        self.content.iter().any(|p| matches!(p, Part::Image(_)))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    pub description: Option<String>,
    pub kind: ToolKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolKind {
    Function {
        schema: Value,
        strict: Option<bool>,
    },
    /// 输入是一段原文。`format` 是语法约束（lark / regex），只有 OpenAI 认
    Freeform {
        format: Option<Value>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Named(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reasoning {
    /// `false` 是客户端明确关掉了推理
    pub enabled: bool,
    pub effort: Option<Effort>,
    /// 推理 token 预算
    pub budget: Option<u64>,
    /// 客户端要不要看到推理内容（摘要）
    pub summary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effort {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Format {
    JsonObject,
    JsonSchema {
        name: Option<String>,
        schema: Value,
        strict: Option<bool>,
    },
}

// ───────────────────────────────────────────────────────── 推理签名

/// 推理内容的签名。**只有签发它的那家认。**
///
/// 客户端要把推理内容原样带回来，下一轮才能接上：Anthropic 在手动思考模式下要求
/// 工具调用那一轮以思考块开头，OpenAI 靠 `encrypted_content` 接续推理。转换后签名
/// 要放进客户端格式的某个字段里带出去：Anthropic 的 `thinking.signature`、Responses
/// 的 `encrypted_content`、Gemini 的 `thoughtSignature`。别家签发的放进去时加
/// `tw1.<厂商>.` 前缀，读回来时认前缀。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub vendor: Vendor,
    /// OpenAI 的是 `推理项 id:encrypted_content`，因为带回去时两样都要
    pub value: String,
    /// Anthropic 的 `redacted_thinking`
    pub redacted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Anthropic,
    OpenAi,
    Google,
}

/// 转换写出去的签名的前缀。**直通时看到它就知道这段推理不是那个上游签发的**
pub const CARRIED: &str = "tw1.";

impl Signature {
    pub fn new(vendor: Vendor, value: impl Into<String>) -> Signature {
        Signature {
            vendor,
            value: value.into(),
            redacted: false,
        }
    }

    fn tag(&self) -> &'static str {
        match (self.vendor, self.redacted) {
            (Vendor::Anthropic, false) => "a",
            (Vendor::Anthropic, true) => "ar",
            (Vendor::OpenAi, _) => "o",
            (Vendor::Google, _) => "g",
        }
    }

    /// 放进 `carrier` 那家的签名字段时的写法。同一家的原样放
    pub fn carried_in(&self, carrier: Vendor) -> String {
        if self.vendor == carrier && !self.redacted {
            self.value.clone()
        } else {
            format!("{CARRIED}{}.{}", self.tag(), self.value)
        }
    }

    /// 从 `carrier` 那家的签名字段读回。没有前缀的就是那家自己签发的
    pub fn read(raw: &str, carrier: Vendor) -> Option<Signature> {
        let Some(rest) = raw.strip_prefix(CARRIED) else {
            return (!raw.is_empty()).then(|| Signature::new(carrier, raw));
        };
        let (tag, value) = rest.split_once('.')?;
        let (vendor, redacted) = match tag {
            "a" => (Vendor::Anthropic, false),
            "ar" => (Vendor::Anthropic, true),
            "o" => (Vendor::OpenAi, false),
            "g" => (Vendor::Google, false),
            // `n`：没有签名的推理（比如 DeepSeek 的 reasoning_content）
            _ => return None,
        };
        Some(Signature {
            vendor,
            value: value.to_string(),
            redacted,
        })
    }
}

/// 没有签名的推理内容在客户端格式里写什么。
///
/// 写一个带前缀的占位，而不是空串：客户端带回来时，直通到真正的上游之前能认出来
/// 并去掉 —— 空串会被 Anthropic 当成签名不合法。
pub fn unsigned_marker() -> String {
    format!("{CARRIED}n.")
}

// ───────────────────────────────────────────────────────── 响应

/// 一次完整的响应。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Response {
    pub id: Option<String>,
    pub model: Option<String>,
    pub blocks: Vec<Block>,
    pub stop: Option<StopReason>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Text(String),
    Thinking(Thinking),
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence(Option<String>),
    ToolUse,
    ContentFilter,
    Refusal,
    /// 上下文窗口满了
    ContextWindow,
    /// 服务端暂停了一个长回合，客户端应当接着请求（Anthropic 的 `pause_turn`）
    Paused,
    /// 上游给了一个没有对应物的原因，原文保留
    Other(String),
}

/// 用量。**`input` 不含缓存读写**，和 Anthropic 的语义一致：三者不重叠，
/// 加起来才是全部输入。`output` 含推理。
///
/// 方言转换和计费读的是同一个：各家响应里的 `usage` 由各自模块的 `usage()` 解析，
/// 旁路嗅探（[`crate::usage::Sniffer`]）也调它们，每条换算只写一处。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// 缓存写用的是 1 小时 TTL 吗。**差价接近一倍**，
    /// 而上游只在 Anthropic 那边给这个细分
    pub cache_1h: bool,
    pub output: u64,
    /// `output` 里有多少是推理
    pub reasoning: u64,
}

impl Usage {
    pub fn is_empty(&self) -> bool {
        *self == Usage::default()
    }

    pub fn prompt_total(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }

    /// 合并同一次响应里分几次报的用量：每项取较大值。Anthropic 的 `message_start`
    /// 报输入、`message_delta` 报累计输出，都只带自己那部分
    pub fn merge(&mut self, other: &Usage) {
        self.input = self.input.max(other.input);
        self.cache_read = self.cache_read.max(other.cache_read);
        self.cache_write = self.cache_write.max(other.cache_write);
        self.cache_1h |= other.cache_1h;
        self.output = self.output.max(other.output);
        self.reasoning = self.reasoning.max(other.reasoning);
    }
}

// ───────────────────────────────────────────────────────── 流

/// 流式响应里的一件事。
///
/// **块有明确的开始和结束**，即使上游格式没有（Chat 和 Gemini 的流里文本和工具调用
/// 混在一起）：解析器负责判断边界，写出器只管按顺序写。`index` 只是块的标识，
/// 写出器各自重新编号。
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Start {
        id: Option<String>,
        model: Option<String>,
    },
    BlockStart {
        index: usize,
        kind: BlockKind,
    },
    Delta {
        index: usize,
        delta: Delta,
    },
    BlockStop {
        index: usize,
    },
    /// 目前为止的用量，累计值
    Usage(Usage),
    Stop(StopReason),
    /// 上游在流里报错
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum BlockKind {
    Text,
    Thinking,
    ToolCall { id: String, name: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    Text(String),
    Thinking(String),
    Signature(Signature),
    /// 函数工具是 JSON 片段，自由格式工具是原文片段
    ToolInput(String),
}

// ───────────────────────────────────────────────────────── 丢弃记录

/// 编码时才发现目标格式表达不了的东西。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    Temperature,
    TopP,
    TopK,
    Stop,
    Seed,
    PresencePenalty,
    FrequencyPenalty,
    ParallelToolCalls,
    /// 推理的开关与强度
    Reasoning,
    /// 历史消息里别家签发的推理内容
    ReasoningHistory,
    Format,
    /// 以 URL 给的图片或文件，目标只收内联数据
    MediaUrl,
    /// 目标不收这种文件
    File,
    /// 工具结果里的图片
    ToolResultImage,
    /// 自由格式工具的语法约束
    FreeformFormat,
}

impl Feature {
    /// 这个东西在客户端请求里叫什么
    pub fn path(self, client: Dialect) -> &'static str {
        use Dialect::*;
        use Feature::*;
        match (self, client) {
            (Temperature, Gemini) => "generationConfig.temperature",
            (Temperature, _) => "temperature",
            (TopP, Gemini) => "generationConfig.topP",
            (TopP, _) => "top_p",
            (TopK, Gemini) => "generationConfig.topK",
            (TopK, _) => "top_k",
            (Stop, Anthropic) => "stop_sequences",
            (Stop, Gemini) => "generationConfig.stopSequences",
            (Stop, _) => "stop",
            (Seed, Gemini) => "generationConfig.seed",
            (Seed, _) => "seed",
            (PresencePenalty, Gemini) => "generationConfig.presencePenalty",
            (PresencePenalty, _) => "presence_penalty",
            (FrequencyPenalty, Gemini) => "generationConfig.frequencyPenalty",
            (FrequencyPenalty, _) => "frequency_penalty",
            (ParallelToolCalls, Anthropic) => "tool_choice.disable_parallel_tool_use",
            (ParallelToolCalls, _) => "parallel_tool_calls",
            (Reasoning, Anthropic) => "thinking",
            (Reasoning, Chat) => "reasoning_effort",
            (Reasoning, Responses) => "reasoning",
            (Reasoning, Gemini) => "generationConfig.thinkingConfig",
            (Reasoning, Bedrock) => "additionalModelRequestFields.thinking",
            (ReasoningHistory, Anthropic) => "messages.content.thinking",
            (ReasoningHistory, Chat) => "messages.reasoning_content",
            (ReasoningHistory, Responses) => "input.reasoning",
            (ReasoningHistory, Gemini) => "contents.parts.thought",
            (ReasoningHistory, Bedrock) => "messages.content.reasoningContent",
            (Format, Anthropic) => "output_config.format",
            (Format, Chat) => "response_format",
            (Format, Responses) => "text.format",
            (Format, Gemini) => "generationConfig.responseJsonSchema",
            (Format, Bedrock) => "outputConfig.textFormat",
            (MediaUrl, Anthropic) => "messages.content.image.source.url",
            (MediaUrl, Chat) => "messages.content.image_url",
            (MediaUrl, Responses) => "input.content.input_image.image_url",
            (MediaUrl, Gemini) => "contents.parts.fileData",
            (MediaUrl, Bedrock) => "messages.content.image.source.s3Location",
            (File, Anthropic) => "messages.content.document",
            (File, Chat) => "messages.content.file",
            (File, Responses) => "input.content.input_file",
            (File, Gemini) => "contents.parts.inlineData",
            (File, Bedrock) => "messages.content.document",
            (ToolResultImage, Anthropic) => "messages.content.tool_result.content.image",
            (ToolResultImage, Chat) => "messages.content",
            (ToolResultImage, Responses) => "input.function_call_output.output.input_image",
            (ToolResultImage, Gemini) => "contents.parts.functionResponse.parts",
            (ToolResultImage, Bedrock) => "messages.content.toolResult.content.image",
            (FreeformFormat, _) => "tools.custom.format",
        }
    }
}

/// 转不过去、被丢掉的东西。**必须交出去给用户看。**
///
/// 记的是字段路径（`thinking`、`tools.web_search`），不是一句解释 —— 「目标格式
/// 没有这个字段」对每一项都成立，由界面说一次就够了。
#[derive(Debug, Clone, PartialEq)]
pub struct Dropped {
    client: Dialect,
    paths: Vec<String>,
}

impl Dropped {
    pub fn new(client: Dialect) -> Dropped {
        Dropped {
            client,
            paths: Vec::new(),
        }
    }

    /// 解码时丢掉的，直接给路径
    pub fn path(&mut self, path: impl Into<String>) {
        let path = path.into();
        if !self.paths.contains(&path) {
            self.paths.push(path);
        }
    }

    /// 编码时丢掉的，按客户端格式换成路径
    pub fn feature(&mut self, f: Feature) {
        self.path(f.path(self.client));
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn into_vec(self) -> Vec<String> {
        self.paths
    }

    pub fn as_slice(&self) -> &[String] {
        &self.paths
    }
}

/// 请求转不了。给客户端的说明，写成一句完整的话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection(pub String);

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Rejection {}

// ───────────────────────────────────────────────────────── 小工具

/// 生成一个 id：`前缀` + 24 位十六进制。**只保证同一进程内不重复** —— 工具调用 id
/// 和响应 id 只需要在一段对话里唯一。
pub fn new_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{prefix}{:016x}{:08x}", t, n as u32)
}

pub fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

pub(crate) fn u64_of(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

pub(crate) fn f64_of(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}

pub(crate) fn arr_of<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// 字段存在且不是 null
pub(crate) fn has(v: &Value, key: &str) -> bool {
    v.get(key).is_some_and(|x| !x.is_null())
}

/// 一个 `string | [{type:"text", text}]` 形状的内容里的文字
pub(crate) fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| str_of(p, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// 同一角色的相邻消息并成一条：Anthropic 和 Gemini 都要求角色交替。
pub fn merge_roles(messages: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for m in messages {
        if m.parts.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.role == m.role => last.parts.extend(m.parts),
            _ => out.push(m),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signature_goes_out_tagged_and_comes_back_as_the_vendor_that_signed_it() {
        let s = Signature::new(Vendor::Anthropic, "EqQBCkY");
        // 放回同一家：原样
        assert_eq!(s.carried_in(Vendor::Anthropic), "EqQBCkY");
        // 放进别家：带前缀，读回来认得出是 Anthropic 的
        let carried = s.carried_in(Vendor::OpenAi);
        assert_eq!(carried, "tw1.a.EqQBCkY");
        assert_eq!(Signature::read(&carried, Vendor::OpenAi), Some(s));
        // 没有前缀的是载体那家自己的
        assert_eq!(
            Signature::read("gAAAA", Vendor::OpenAi),
            Some(Signature::new(Vendor::OpenAi, "gAAAA"))
        );
    }

    #[test]
    fn a_redacted_block_keeps_being_redacted_even_back_home() {
        let s = Signature {
            vendor: Vendor::Anthropic,
            value: "data".into(),
            redacted: true,
        };
        let carried = s.carried_in(Vendor::Anthropic);
        assert_eq!(carried, "tw1.ar.data");
        assert_eq!(Signature::read(&carried, Vendor::Anthropic), Some(s));
    }

    #[test]
    fn the_unsigned_marker_reads_back_as_no_signature() {
        assert_eq!(Signature::read(&unsigned_marker(), Vendor::Anthropic), None);
        assert_eq!(Signature::read("", Vendor::Anthropic), None);
    }

    #[test]
    fn a_data_uri_splits_into_mime_and_data_and_back() {
        let m = Media::from_uri("data:image/png;base64,iVBOR");
        assert_eq!(
            m,
            Media::Base64 {
                mime: "image/png".into(),
                data: "iVBOR".into()
            }
        );
        assert_eq!(m.to_uri(), "data:image/png;base64,iVBOR");
        assert_eq!(
            Media::from_uri("https://x/a.png"),
            Media::Url("https://x/a.png".into())
        );
    }

    #[test]
    fn a_dropped_feature_is_named_the_way_the_client_wrote_it() {
        let mut d = Dropped::new(Dialect::Gemini);
        d.feature(Feature::TopK);
        d.feature(Feature::TopK);
        assert_eq!(d.into_vec(), ["generationConfig.topK"]);
        let mut d = Dropped::new(Dialect::Anthropic);
        d.feature(Feature::Stop);
        assert_eq!(d.into_vec(), ["stop_sequences"]);
    }

    #[test]
    fn freeform_input_rides_in_an_object_where_only_json_tools_exist() {
        let t = ToolInput::Text("*** Begin Patch".into());
        assert_eq!(t.to_object()["input"], "*** Begin Patch");
        assert_eq!(
            ToolInput::Json(serde_json::json!([1])).to_object(),
            serde_json::json!({"value": [1]})
        );
    }

    #[test]
    fn unparseable_tool_arguments_are_kept_as_text() {
        assert_eq!(
            ToolInput::from_json_text("不是 JSON"),
            ToolInput::Json(Value::String("不是 JSON".into()))
        );
        assert_eq!(
            ToolInput::from_json_text(""),
            ToolInput::Json(serde_json::json!({}))
        );
    }

    #[test]
    fn adjacent_turns_of_the_same_role_merge_and_empty_ones_vanish() {
        let m = |role, t: &str| Message {
            role,
            parts: if t.is_empty() {
                vec![]
            } else {
                vec![Part::Text(t.into())]
            },
        };
        let out = merge_roles(vec![
            m(Role::User, "a"),
            m(Role::Assistant, ""),
            m(Role::User, "b"),
            m(Role::Assistant, "c"),
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].parts.len(), 2);
    }

    #[test]
    fn usage_reported_in_pieces_merges_to_the_largest_of_each() {
        let mut u = Usage {
            input: 100,
            cache_read: 50,
            output: 1,
            ..Default::default()
        };
        u.merge(&Usage {
            output: 40,
            ..Default::default()
        });
        assert_eq!((u.input, u.cache_read, u.output), (100, 50, 40));
        assert_eq!(u.prompt_total(), 150);
    }
}
