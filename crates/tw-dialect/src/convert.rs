//! 一次转换的入口：请求怎么改写，响应怎么改回来。
//!
//! ```text
//! 客户端请求 ──decode──▶ 中间表示 ──encode──▶ 上游请求          prepare()
//! 客户端响应 ◀─encode─── 中间表示 ◀─decode─── 上游响应          Session::response()
//! 客户端的流 ◀─write──── 事件     ◀─parse──── 上游的流          Session::stream()
//! 客户端响应 ◀─encode─── 聚合     ◀─parse──── 上游的流          Session::collector()
//! ```
//!
//! **同格式不经过这里**：客户端和上游说同一种格式时，网关原样转发，一个字节都不改。
//! 唯一的例外见 [`strip_carried`]。

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::frame::{self, Frame};
use crate::ir::*;
use crate::{anthropic, chat, gemini, responses};

/// 改写好的上游请求。
#[derive(Debug, Clone)]
pub struct Prepared {
    pub body: Vec<u8>,
    /// 上游路径，接在 base_url 后面
    pub path: String,
    /// 上游查询串，不含 `?`
    pub query: Option<String>,
    /// 客户端请求里转不过去、被丢掉的字段
    pub dropped: Vec<String>,
    pub session: Session,
}

/// 客户端请求 → 上游请求。
///
/// `path` 和 `query` 是客户端请求的：Gemini 把模型和是否流式写在路径里。
pub fn prepare(
    client: Dialect,
    body: &[u8],
    path: &str,
    query: Option<&str>,
    target: &Target,
) -> Result<Prepared, Rejection> {
    let v: Value =
        serde_json::from_slice(body).map_err(|_| Rejection("请求体不是合法的 JSON。".into()))?;
    let mut dropped = Dropped::new(client);
    let mut shape = ClientShape::default();
    let request = match client {
        Dialect::Anthropic => anthropic::decode_request(&v, &mut dropped)?,
        Dialect::Chat => chat::decode_request(&v, &mut dropped, &mut shape)?,
        Dialect::Responses => responses::decode_request(&v, &mut dropped, &mut shape)?,
        Dialect::Gemini => {
            let (model, stream) = gemini_path(path).ok_or_else(|| {
                Rejection(format!(
                    "无法从路径 {path} 中读出 Gemini 的模型和调用方式。"
                ))
            })?;
            shape.gemini_sse = query.is_some_and(|q| q.split('&').any(|kv| kv == "alt=sse"));
            gemini::decode_request(&v, &model, stream, &mut dropped)?
        }
    };

    let (body, path, query) = match target.dialect {
        Dialect::Anthropic => (
            anthropic::encode_request(&request, target, &mut dropped),
            "/v1/messages".to_string(),
            None,
        ),
        Dialect::Chat => (
            chat::encode_request(&request, target, &mut dropped),
            "/v1/chat/completions".to_string(),
            None,
        ),
        Dialect::Responses => (
            responses::encode_request(&request, target, &mut dropped),
            "/v1/responses".to_string(),
            None,
        ),
        Dialect::Gemini => {
            let model = request
                .model
                .strip_prefix("models/")
                .unwrap_or(&request.model);
            let (action, query) = if request.stream {
                ("streamGenerateContent", Some("alt=sse".to_string()))
            } else {
                ("generateContent", None)
            };
            (
                gemini::encode_request(&request, target, &mut dropped),
                format!("/v1beta/models/{model}:{action}"),
                query,
            )
        }
    };
    Ok(Prepared {
        body: body.to_string().into_bytes(),
        path,
        query,
        dropped: dropped.into_vec(),
        session: Session::new(client, target.dialect, &request, shape),
    })
}

/// `/v1beta/models/gemini-2.5-pro:streamGenerateContent` → (模型, 是否流式)
fn gemini_path(path: &str) -> Option<(String, bool)> {
    let (_, rest) = path.split_once("/models/")?;
    let (model, action) = rest.rsplit_once(':')?;
    let stream = match action {
        "streamGenerateContent" => true,
        "generateContent" => false,
        _ => return None,
    };
    Some((model.to_string(), stream))
}

/// 一次请求转换之后，写响应时要用的东西。
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub client: Dialect,
    pub upstream: Dialect,
    /// 客户端请求的模型名。上游响应里没写模型时用它
    pub model: String,
    /// 客户端要的是流
    pub stream: bool,
    /// 客户端定义成自由格式的工具
    pub(crate) freeform: HashSet<String>,
    /// 客户端的写法细节
    pub(crate) shape: ClientShape,
}

impl Session {
    pub(crate) fn new(
        client: Dialect,
        upstream: Dialect,
        r: &Request,
        shape: ClientShape,
    ) -> Session {
        Session {
            client,
            upstream,
            model: r.model.clone(),
            stream: r.stream,
            freeform: r
                .tools
                .iter()
                .filter(|t| matches!(t.kind, ToolKind::Freeform { .. }))
                .map(|t| t.name.clone())
                .collect(),
            shape,
        }
    }

    /// 这个工具在客户端那边是自由格式的
    pub(crate) fn is_freeform(&self, name: &str) -> bool {
        self.freeform.contains(name)
    }

    /// Responses 客户端的 namespace 工具：展开后的名字 → (namespace, 名字)
    pub(crate) fn namespaced(&self, name: &str) -> Option<&(String, String)> {
        self.shape.namespaced.get(name)
    }

    /// 上游的整包响应 → 客户端的整包响应。上游返回的不是 JSON 时是 `None`
    pub fn response(&self, body: &[u8]) -> Option<Vec<u8>> {
        let v: Value = serde_json::from_slice(body).ok()?;
        let mut r = match self.upstream {
            Dialect::Anthropic => anthropic::decode_response(&v),
            Dialect::Chat => chat::decode_response(&v),
            Dialect::Responses => responses::decode_response(&v),
            Dialect::Gemini => gemini::decode_response(&v),
        };
        self.normalize(&mut r);
        Some(self.encode(&r))
    }

    fn encode(&self, r: &Response) -> Vec<u8> {
        let v = match self.client {
            Dialect::Anthropic => anthropic::encode_response(r, self),
            Dialect::Chat => chat::encode_response(r, self),
            Dialect::Responses => responses::encode_response(r, self),
            Dialect::Gemini => gemini::encode_response(r, self),
        };
        v.to_string().into_bytes()
    }

    /// 上游的错误响应 → 客户端格式的错误体。状态码不变，说明取上游的原话
    pub fn error(&self, status: u16, body: &[u8]) -> Vec<u8> {
        let message = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|v| match self.upstream {
                Dialect::Anthropic => anthropic::response::error_message(&v),
                Dialect::Chat | Dialect::Responses => chat::response::error_message(&v),
                Dialect::Gemini => gemini::response::error_message(&v),
            })
            .unwrap_or_else(|| {
                let text = String::from_utf8_lossy(body);
                let text = text.trim();
                if text.is_empty() {
                    format!("上游返回 HTTP {status}，没有附带说明。")
                } else {
                    text.chars().take(2000).collect()
                }
            });
        error_body(self.client, status, &message)
    }

    /// 上游的流 → 客户端的流
    pub fn stream(&self) -> StreamConverter {
        StreamConverter {
            decoder: frame::Decoder::default(),
            parser: Parser::new(self.upstream),
            normalizer: Normalizer::new(self),
            writer: Writer::new(self),
            finished: false,
        }
    }

    /// 上游只给流、客户端要整包时，把流收成一个响应
    pub fn collector(&self) -> Collector {
        Collector {
            session: self.clone(),
            decoder: frame::Decoder::default(),
            parser: Parser::new(self.upstream),
            normalizer: Normalizer::new(self),
            response: Response::default(),
            open: HashMap::new(),
            error: None,
        }
    }

    /// 自由格式工具的输入：别家上游把它包在 `{"input": …}` 里，拆出来
    fn normalize(&self, r: &mut Response) {
        for b in &mut r.blocks {
            if let Block::ToolCall(c) = b
                && self.is_freeform(&c.name)
                && let ToolInput::Json(v) = &c.input
            {
                c.input = ToolInput::Text(unwrap_freeform(&v.to_string()));
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(client: Dialect, upstream: Dialect) -> Session {
        Session {
            client,
            upstream,
            model: "m".into(),
            stream: true,
            freeform: HashSet::new(),
            shape: ClientShape {
                namespaced: HashMap::new(),
                include_usage: false,
                gemini_sse: true,
            },
        }
    }
}

/// 给某种格式的客户端的错误体。
pub fn error_body(client: Dialect, status: u16, message: &str) -> Vec<u8> {
    let v = match client {
        Dialect::Anthropic => anthropic::response::error_body(status, message),
        Dialect::Chat | Dialect::Responses => chat::response::error_body(status, message),
        Dialect::Gemini => gemini::response::error_body(status, message),
    };
    v.to_string().into_bytes()
}

fn unwrap_freeform(json_text: &str) -> String {
    match serde_json::from_str::<Value>(json_text) {
        Ok(Value::Object(o)) => match o.get("input") {
            Some(Value::String(s)) => s.clone(),
            _ => json_text.to_string(),
        },
        Ok(Value::String(s)) => s,
        _ => json_text.to_string(),
    }
}

// ───────────────────────────────────────────────────────── 流

enum Parser {
    Anthropic(anthropic::stream::Parser),
    Chat(chat::stream::Parser),
    Responses(responses::stream::Parser),
    Gemini(gemini::stream::Parser),
}

impl Parser {
    fn new(upstream: Dialect) -> Parser {
        match upstream {
            Dialect::Anthropic => Parser::Anthropic(Default::default()),
            Dialect::Chat => Parser::Chat(Default::default()),
            Dialect::Responses => Parser::Responses(Default::default()),
            Dialect::Gemini => Parser::Gemini(Default::default()),
        }
    }

    fn frame(&mut self, f: &Frame, out: &mut Vec<Event>) {
        match self {
            Parser::Anthropic(p) => p.frame(f, out),
            Parser::Chat(p) => p.frame(f, out),
            Parser::Responses(p) => p.frame(f, out),
            Parser::Gemini(p) => p.frame(f, out),
        }
    }

    fn finish(&mut self, out: &mut Vec<Event>) {
        match self {
            Parser::Anthropic(_) => {}
            Parser::Chat(p) => p.finish(out),
            Parser::Responses(p) => p.finish(out),
            Parser::Gemini(p) => p.finish(out),
        }
    }
}

enum Writer {
    Anthropic(anthropic::stream::Writer),
    Chat(chat::stream::Writer),
    Responses(responses::stream::Writer),
    Gemini(gemini::stream::Writer),
}

impl Writer {
    fn new(s: &Session) -> Writer {
        match s.client {
            Dialect::Anthropic => Writer::Anthropic(anthropic::stream::Writer::new(s)),
            Dialect::Chat => Writer::Chat(chat::stream::Writer::new(s)),
            Dialect::Responses => Writer::Responses(responses::stream::Writer::new(s)),
            Dialect::Gemini => Writer::Gemini(gemini::stream::Writer::new(s)),
        }
    }

    fn event(&mut self, e: &Event) -> String {
        match self {
            Writer::Anthropic(w) => w.event(e),
            Writer::Chat(w) => w.event(e),
            Writer::Responses(w) => w.event(e),
            Writer::Gemini(w) => w.event(e),
        }
    }

    fn finish(&mut self) -> String {
        match self {
            Writer::Anthropic(w) => w.finish(),
            Writer::Chat(w) => w.finish(),
            Writer::Responses(w) => w.finish(),
            Writer::Gemini(w) => w.finish(),
        }
    }
}

/// 解析器和写出器之间的两处修正。
///
/// - **自由格式工具**：别家上游把原文包在 `{"input": …}` 里分片发来，攒到块结束拆出原文
/// - **没有参数的函数调用**：有的上游一个参数片段都不发，补一个 `{}`，否则客户端解析
///   空串会失败
struct Normalizer {
    freeform: HashSet<String>,
    /// 上游把自由格式的输入包成 JSON（除 Responses 以外都是）
    wrapped: bool,
    buffers: HashMap<usize, String>,
    /// 开着的函数调用块 → 收到过参数没有
    calls: HashMap<usize, bool>,
}

impl Normalizer {
    fn new(s: &Session) -> Normalizer {
        Normalizer {
            freeform: s.freeform.clone(),
            wrapped: s.upstream != Dialect::Responses,
            buffers: HashMap::new(),
            calls: HashMap::new(),
        }
    }

    fn apply(&mut self, e: Event, out: &mut Vec<Event>) {
        match &e {
            Event::BlockStart {
                index,
                kind: BlockKind::ToolCall { name, .. },
            } => {
                if self.freeform.contains(name) {
                    if self.wrapped {
                        self.buffers.insert(*index, String::new());
                    }
                } else {
                    self.calls.insert(*index, false);
                }
            }
            Event::Delta {
                index,
                delta: Delta::ToolInput(p),
            } => {
                if let Some(buf) = self.buffers.get_mut(index) {
                    buf.push_str(p);
                    return;
                }
                if let Some(seen) = self.calls.get_mut(index) {
                    *seen |= !p.is_empty();
                }
            }
            Event::BlockStop { index } => {
                if let Some(buf) = self.buffers.remove(index) {
                    out.push(Event::Delta {
                        index: *index,
                        delta: Delta::ToolInput(unwrap_freeform(&buf)),
                    });
                }
                if self.calls.remove(index) == Some(false) {
                    out.push(Event::Delta {
                        index: *index,
                        delta: Delta::ToolInput("{}".into()),
                    });
                }
            }
            _ => {}
        }
        out.push(e);
    }
}

/// 上游的流 → 客户端的流。**边收边转**，不整块缓冲。
pub struct StreamConverter {
    decoder: frame::Decoder,
    parser: Parser,
    normalizer: Normalizer,
    writer: Writer,
    finished: bool,
}

impl StreamConverter {
    fn run(&mut self, frames: Vec<Frame>, finish: bool) -> Vec<u8> {
        let mut raw = Vec::new();
        for f in &frames {
            self.parser.frame(f, &mut raw);
        }
        if finish {
            self.parser.finish(&mut raw);
        }
        let mut events = Vec::with_capacity(raw.len());
        for e in raw {
            self.normalizer.apply(e, &mut events);
        }
        let mut out = String::new();
        for e in &events {
            out.push_str(&self.writer.event(e));
        }
        out.into_bytes()
    }

    /// 喂一块上游字节，吐出转好的客户端字节
    pub fn process(&mut self, chunk: &[u8]) -> Vec<u8> {
        let frames = self.decoder.feed(chunk);
        self.run(frames, false)
    }

    /// 上游的流结束了（正常结束或断开）。**幂等**
    pub fn finish(&mut self) -> Vec<u8> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let frames = self.decoder.flush();
        let mut out = self.run(frames, true);
        out.extend(self.writer.finish().into_bytes());
        out
    }
}

/// 上游的流 → 一个整包响应。
pub struct Collector {
    session: Session,
    decoder: frame::Decoder,
    parser: Parser,
    normalizer: Normalizer,
    response: Response,
    /// 中间表示的块 → 在 `response.blocks` 里的位置
    open: HashMap<usize, usize>,
    error: Option<String>,
}

impl Collector {
    pub fn process(&mut self, chunk: &[u8]) {
        let frames = self.decoder.feed(chunk);
        self.run(frames, false);
    }

    fn run(&mut self, frames: Vec<Frame>, finish: bool) {
        let mut raw = Vec::new();
        for f in &frames {
            self.parser.frame(f, &mut raw);
        }
        if finish {
            self.parser.finish(&mut raw);
        }
        let mut events = Vec::with_capacity(raw.len());
        for e in raw {
            self.normalizer.apply(e, &mut events);
        }
        for e in events {
            self.collect(e);
        }
    }

    fn collect(&mut self, e: Event) {
        let r = &mut self.response;
        match e {
            Event::Start { id, model } => {
                r.id = r.id.take().or(id);
                r.model = r.model.take().or(model);
            }
            Event::BlockStart { index, kind } => {
                self.open.insert(index, r.blocks.len());
                r.blocks.push(match kind {
                    BlockKind::Text => Block::Text(String::new()),
                    BlockKind::Thinking => Block::Thinking(Thinking {
                        text: String::new(),
                        signature: None,
                    }),
                    BlockKind::ToolCall { id, name } => Block::ToolCall(ToolCall {
                        id,
                        name,
                        input: ToolInput::Text(String::new()),
                    }),
                });
            }
            Event::Delta { index, delta } => {
                let Some(block) = self.open.get(&index).and_then(|i| r.blocks.get_mut(*i)) else {
                    return;
                };
                match (block, delta) {
                    (Block::Text(t), Delta::Text(d)) => t.push_str(&d),
                    (Block::Thinking(th), Delta::Thinking(d)) => th.text.push_str(&d),
                    (Block::Thinking(th), Delta::Signature(s)) => th.signature = Some(s),
                    (Block::ToolCall(c), Delta::ToolInput(d)) => {
                        if let ToolInput::Text(t) = &mut c.input {
                            t.push_str(&d);
                        }
                    }
                    _ => {}
                }
            }
            Event::BlockStop { index } => {
                if let Some(i) = self.open.remove(&index)
                    && let Some(Block::ToolCall(c)) = r.blocks.get_mut(i)
                    && !self.session.is_freeform(&c.name)
                    && let ToolInput::Text(t) = &c.input
                {
                    c.input = ToolInput::from_json_text(t);
                }
            }
            Event::Usage(u) => r.usage.get_or_insert_default().merge(&u),
            Event::Stop(s) => r.stop = Some(s),
            Event::Error { message } => self.error = Some(message),
        }
    }

    /// 流结束，写出客户端的整包响应。上游在流里报了错时是 `Err(上游的说明)`
    pub fn finish(mut self) -> Result<Vec<u8>, String> {
        let frames = self.decoder.flush();
        self.run(frames, true);
        if let Some(e) = self.error {
            return Err(e);
        }
        Ok(self.session.encode(&self.response))
    }
}

// ───────────────────────────────────────────────────────── 直通时的清理

/// 直通请求里去掉转换写出去的推理签名。**没有需要去掉的就返回 `None`，请求一个字节都不改。**
///
/// 客户端把转换写出去的推理内容原样带回来（这正是签名存在的意义）。之后如果这段对话
/// 换到了和客户端同格式的上游（故障转移、改了路由），请求会直通过去，而那些带
/// `tw1.` 前缀的签名不是这个上游签发的：Anthropic 会以签名无效拒绝整个请求，
/// 对话从此卡死。所以直通前把它们去掉 —— 那段推理本来就不是这个上游产生的。
pub fn strip_carried(client: Dialect, body: &[u8]) -> Option<Vec<u8>> {
    if !body.windows(CARRIED.len()).any(|w| w == CARRIED.as_bytes()) {
        return None;
    }
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let carried = |x: Option<&Value>| {
        x.and_then(Value::as_str)
            .is_some_and(|s| s.starts_with(CARRIED))
    };
    let mut changed = false;
    match client {
        Dialect::Anthropic => {
            let messages = v.get_mut("messages")?.as_array_mut()?;
            for m in messages.iter_mut() {
                if let Some(blocks) = m.get_mut("content").and_then(Value::as_array_mut) {
                    let before = blocks.len();
                    blocks.retain(|b| {
                        !(b.get("type") == Some(&Value::from("thinking"))
                            && carried(b.get("signature")))
                    });
                    changed |= blocks.len() != before;
                }
            }
            // 只剩推理内容的消息整条去掉
            messages.retain(|m| {
                m.get("content")
                    .and_then(Value::as_array)
                    .is_none_or(|c| !c.is_empty())
            });
        }
        Dialect::Responses => {
            let input = v.get_mut("input")?.as_array_mut()?;
            let before = input.len();
            input.retain(|i| {
                !(i.get("type") == Some(&Value::from("reasoning"))
                    && carried(i.get("encrypted_content")))
            });
            changed = input.len() != before;
        }
        Dialect::Gemini => {
            let contents = v.get_mut("contents")?.as_array_mut()?;
            for c in contents.iter_mut() {
                if let Some(parts) = c.get_mut("parts").and_then(Value::as_array_mut) {
                    let before = parts.len();
                    parts.retain(|p| {
                        !(p.get("thought") == Some(&Value::Bool(true))
                            && carried(
                                p.get("thoughtSignature")
                                    .or_else(|| p.get("thought_signature")),
                            ))
                    });
                    changed |= parts.len() != before;
                }
            }
            contents.retain(|c| {
                c.get("parts")
                    .and_then(Value::as_array)
                    .is_none_or(|p| !p.is_empty())
            });
        }
        Dialect::Chat => return None,
    }
    changed.then(|| v.to_string().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn target(d: Dialect) -> Target {
        Target {
            dialect: d,
            official: true,
            default_max_tokens: 16000,
        }
    }

    #[test]
    fn a_gemini_client_path_gives_the_model_and_stream_and_the_upstream_path_is_built() {
        let body = br#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;
        let p = prepare(
            Dialect::Gemini,
            body,
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent",
            Some("alt=sse&key=x"),
            &target(Dialect::Anthropic),
        )
        .unwrap();
        assert_eq!(p.path, "/v1/messages");
        assert_eq!(p.query, None);
        assert!(p.session.stream && p.session.shape.gemini_sse);
        let v: Value = serde_json::from_slice(&p.body).unwrap();
        assert_eq!(v["model"], "gemini-2.5-pro");
        assert_eq!(v["stream"], true);

        let body = br#"{"model":"models/gemini-3-pro-preview","messages":[{"role":"user","content":"hi"}],"stream":true}"#;
        let p = prepare(
            Dialect::Chat,
            body,
            "/v1/chat/completions",
            None,
            &target(Dialect::Gemini),
        )
        .unwrap();
        assert_eq!(
            p.path,
            "/v1beta/models/gemini-3-pro-preview:streamGenerateContent"
        );
        assert_eq!(p.query.as_deref(), Some("alt=sse"));
    }

    #[test]
    fn a_body_that_is_not_json_is_refused_in_words() {
        let e = prepare(
            Dialect::Anthropic,
            b"{",
            "/v1/messages",
            None,
            &target(Dialect::Chat),
        )
        .unwrap_err();
        assert_eq!(e.0, "请求体不是合法的 JSON。");
    }

    #[test]
    fn an_upstream_error_keeps_its_words_in_the_client_shape() {
        let s = Session::for_test(Dialect::Anthropic, Dialect::Gemini);
        let body = s.error(
            429,
            br#"{"error":{"code":429,"message":"Resource has been exhausted","status":"RESOURCE_EXHAUSTED"}}"#,
        );
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "rate_limit_error");
        assert_eq!(v["error"]["message"], "Resource has been exhausted");

        let s = Session::for_test(Dialect::Gemini, Dialect::Chat);
        let v: Value = serde_json::from_slice(&s.error(502, b"")).unwrap();
        assert_eq!(v["error"]["status"], "INTERNAL");
        assert!(v["error"]["message"].as_str().unwrap().contains("502"));
    }

    #[test]
    fn a_freeform_call_from_a_json_only_upstream_is_unwrapped_in_the_stream() {
        let mut s = Session::for_test(Dialect::Responses, Dialect::Anthropic);
        s.freeform.insert("apply_patch".into());
        let mut c = s.stream();
        let up = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"apply_patch\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"input\\\":\\\"*** Be\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"gin\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        );
        let mut out = c.process(up.as_bytes());
        out.extend(c.finish());
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("\"type\":\"response.custom_tool_call_input.done\""),
            "{text}"
        );
        assert!(text.contains("\"input\":\"*** Begin\""), "{text}");
        assert!(!text.contains("\\\"input\\\""), "包装没有拆掉：{text}");
    }

    #[test]
    fn a_call_without_arguments_gets_an_empty_object() {
        let s = Session::for_test(Dialect::Anthropic, Dialect::Gemini);
        let mut n = Normalizer::new(&s);
        let mut out = Vec::new();
        n.apply(
            Event::BlockStart {
                index: 0,
                kind: BlockKind::ToolCall {
                    id: "c".into(),
                    name: "now".into(),
                },
            },
            &mut out,
        );
        n.apply(Event::BlockStop { index: 0 }, &mut out);
        assert_eq!(
            out[1],
            Event::Delta {
                index: 0,
                delta: Delta::ToolInput("{}".into())
            }
        );
    }

    #[test]
    fn carried_signatures_are_stripped_only_when_present() {
        let clean = br#"{"messages":[{"role":"user","content":"tw1. is just text here"}]}"#;
        assert_eq!(strip_carried(Dialect::Anthropic, clean), None);

        let body = json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "x", "signature": "tw1.o.rs_1:enc"},
                {"type": "text", "text": "answer"}
            ]},
            {"role": "assistant", "content": [{"type": "thinking", "thinking": "y", "signature": "tw1.n."}]}
        ]})
        .to_string();
        let out: Value =
            serde_json::from_slice(&strip_carried(Dialect::Anthropic, body.as_bytes()).unwrap())
                .unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(
            msgs[1]["content"],
            json!([{"type": "text", "text": "answer"}])
        );

        let body = json!({"input": [
            {"type": "reasoning", "id": "rs_tw", "summary": [], "encrypted_content": "tw1.a.sig"},
            {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "gAAA"}
        ]})
        .to_string();
        let out: Value =
            serde_json::from_slice(&strip_carried(Dialect::Responses, body.as_bytes()).unwrap())
                .unwrap();
        assert_eq!(out["input"].as_array().unwrap().len(), 1);
        assert_eq!(out["input"][0]["id"], "rs_1");
    }
}
