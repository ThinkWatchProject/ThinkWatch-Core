//! 回答里的模型名写成客户端用的名称。
//!
//! 发出去的名称 S 和客户端要的 N 不一样时 —— 别名、规则改写、指定模型、插件改名 ——
//! 上游在回答里写的是它自己的那个名称 A。**A 和 S 是同一个模型就写成 N**：客户端问的
//! 是 N，回答说的也该是 N。认法和上游体检同一套（[`tw_pricing::model_name::same`]），
//! 日期版本不同也算同一个。有的客户端拿回答里的名称和请求比，对不上就当成模型被换了
//! （Codex 看 `openai-model` 头）。
//!
//! **A 是别的模型就原样转发**：上游真换了模型（ChatGPT 改道、中转站偷换），客户端必须
//! 看得见。回答里没写模型名（或者写的是空串）的，不替它加。只改成功的回答：上游回的
//! 错误原样交给客户端，那是它的原话。
//!
//! # 在中继上的位置
//!
//! - 留档和嗅探在它之前：请求记录和上游体检看的是上游的原话 A（见 [`crate::ending`]）。
//! - 在格式转换之后：改的是客户端那种格式里的字段。上游没写模型名时转换器填的是 S
//!   （Bedrock 的回答里就没有），一样写成 N。
//! - 在回答钩子之前：插件看不到、也改不了回答里的模型名（它只管文字和工具调用），插件
//!   补出来的帧照抄前一帧的外壳，抄到的已经是 N。插件的 `ctx.model` 照旧是 S、
//!   `ctx.requestedModel` 是 N。
//!
//! # 不缓冲
//!
//! 和 [`tw_dialect::usage`] 的嗅探器同一个办法：边流边数 JSON 的层次，不解析成值。字节
//! 照常往下走，**只扣住模型名那一个字符串**（几十个字节），读完了再决定原样发还是换掉。
//! 认的字段也和嗅探器一样 —— 嗅探器读出来记成 A 的，正是这里换掉的那一个：
//!
//! | | 在哪儿 |
//! |---|---|
//! | Anthropic | 整包的 `model`；流里 `message_start` 的 `message.model` |
//! | OpenAI Chat | 整包和每个 chunk 的 `model` |
//! | OpenAI Responses | 整包的 `model`；流里 `response.created` 这些帧的 `response.model` |
//! | Gemini | 整包和每一帧的 `modelVersion`（`model_version` 也认） |
//!
//! 一帧是 SSE 的一行 `data:`、整包那一个对象、Gemini 不带 `alt=sse` 时那个数组里的每一个
//! 元素，或者 WebSocket 上的一条消息。嵌在别处的同名键（工具参数里的 `model`）不动。

use axum::http::{HeaderMap, HeaderValue};

/// 回答里写着模型名的响应头。OpenAI 和 Codex 后端都发；Codex 拿它和请求的模型比
pub const HEADERS: &[&str] = &["openai-model", "x-openai-model"];

/// 模型名最多扣住多长。再长就不是一个模型名，原样放过去
const MAX_MODEL_LEN: usize = 256;

/// JSON 最多数到第几层。再深的只数不记：根对象和它下面那一层早就过去了
const MAX_DEPTH: usize = 64;

/// 客户端用的名称和发出去的名称。
#[derive(Debug, Clone)]
pub struct Rename {
    /// N：客户端请求里写的
    client: String,
    /// S：发给回答它的那一家的
    sent: String,
}

impl Rename {
    /// 两个名称一样（或者客户端没写模型名）时不用改，是 None。
    pub fn new(client: &str, sent: &str) -> Option<Self> {
        (!client.is_empty() && client != sent).then(|| Self {
            client: client.to_string(),
            sent: sent.to_string(),
        })
    }

    /// 上游写的这个名称要不要换成客户端的：和发出去的是同一个模型才换。空的不算写了
    pub fn wants(&self, answered: &str) -> bool {
        let a = answered.trim();
        !a.is_empty() && a != self.client && tw_pricing::model_name::same(a, &self.sent)
    }

    /// 响应头里的模型名（[`HEADERS`]）按同一条规矩换。
    pub fn headers(&self, headers: &mut HeaderMap) {
        for name in HEADERS {
            let Some(v) = headers.get(*name) else {
                continue;
            };
            let wants = std::str::from_utf8(v.as_bytes()).is_ok_and(|a| self.wants(a));
            // 名称里有头里放不下的字符（换行这类）时不换：宁可原样，也不发一个坏的头
            if wants && let Ok(n) = HeaderValue::from_bytes(self.client.as_bytes()) {
                headers.insert(*name, n);
            }
        }
    }

    /// 边流边改回答正文的那一个。
    pub fn body(self) -> Body {
        let quoted = serde_json::to_vec(&self.client).unwrap_or_default();
        Body {
            names: self,
            quoted,
            last: None,
            stack: Vec::new(),
            deep: 0,
            in_str: false,
            escaped: false,
            capture: Capture::Skip,
            key: Vec::new(),
            held: Vec::new(),
            line: Some(0),
        }
    }
}

/// 回答正文里的模型名：边流边换。
///
/// 喂进来的是客户端将要收到的字节（SSE、JSON 数组流、整包都行），吐出来的是换过的。
/// 除了正在读的模型名那一个字符串，什么都不扣。
#[derive(Debug)]
pub struct Body {
    names: Rename,
    /// N 写成 JSON 字符串的样子（带引号、转义过）
    quoted: Vec<u8>,
    /// 上一次见到的模型名（原样的字节）和换不换。Chat 的每个 chunk 都写一遍同一个名字，
    /// 不必每一帧都归一一遍
    last: Option<(Vec<u8>, bool)>,
    stack: Vec<Scope>,
    /// 超过 [`MAX_DEPTH`] 的那几层：只数，不记
    deep: usize,
    in_str: bool,
    escaped: bool,
    capture: Capture,
    /// 正在读的键（只有根和 `Inner` 那两层的键要认）
    key: Vec<u8>,
    /// 扣住的模型名：从开头的引号起
    held: Vec<u8>,
    /// 这一行开头对上了 `data:` 的几个字节；不在行首、或者已经对不上了是 None。
    /// **对上了就是新的一帧**，层次从零数起（理由同 [`tw_dialect::usage`] 的嗅探器）
    line: Option<usize>,
}

/// 数到的一层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Obj {
        at: Level,
        /// 刚读到的键是哪一个（冒号后面那个值归它）
        key: Key,
        /// 下一个字符串是键
        want_key: bool,
    },
    Arr,
}

/// 一个对象在一帧里的位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    /// 帧的根
    Root,
    /// 根下 `message` / `response` 的那个对象
    Inner,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    None,
    /// 这一层的模型名
    Model,
    /// 根下的 `message` / `response`：它的值是 [`Level::Inner`] 那一层
    Wrapper,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capture {
    /// 原样过去
    Skip,
    /// 一个键：原样过去，同时记下是哪一个
    Key,
    /// 模型名：扣住，读完再定
    Value,
}

impl Body {
    /// 改一段。返回现在该发的字节：模型名读到一半的话，那一截留到下一段。
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(chunk.len() + self.quoted.len());
        let mut i = 0;
        while i < chunk.len() {
            if self.in_str {
                i = self.in_string(chunk, i, &mut out);
                continue;
            }
            let c = chunk[i];
            i += 1;
            self.at_line_start(c);
            if c == b'"' {
                self.open_string(&mut out);
                continue;
            }
            out.push(c);
            match c {
                b':' => {
                    if let Some(Scope::Obj { want_key, .. }) = self.top() {
                        *want_key = false;
                    }
                }
                b',' => {
                    if let Some(Scope::Obj { want_key, key, .. }) = self.top() {
                        *want_key = true;
                        *key = Key::None;
                    }
                }
                b'{' => {
                    let at = match (self.deep, self.stack.as_slice()) {
                        (0, [] | [Scope::Arr]) => Level::Root,
                        (
                            0,
                            [
                                ..,
                                Scope::Obj {
                                    at: Level::Root,
                                    key: Key::Wrapper,
                                    want_key: false,
                                },
                            ],
                        ) => Level::Inner,
                        _ => Level::Other,
                    };
                    self.push(Scope::Obj {
                        at,
                        key: Key::None,
                        want_key: true,
                    });
                }
                b'[' => self.push(Scope::Arr),
                b'}' | b']' => {
                    if self.deep > 0 {
                        self.deep -= 1;
                    } else {
                        self.stack.pop();
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// 流结束了（或者断了）：扣着的那半个名字原样交出去，从头数起。
    pub fn flush(&mut self) -> Vec<u8> {
        self.stack.clear();
        self.deep = 0;
        self.in_str = false;
        self.escaped = false;
        self.capture = Capture::Skip;
        self.key.clear();
        self.line = Some(0);
        std::mem::take(&mut self.held)
    }

    /// 当前这一层。太深、只数不记的时候没有
    fn top(&mut self) -> Option<&mut Scope> {
        if self.deep > 0 {
            return None;
        }
        self.stack.last_mut()
    }

    fn push(&mut self, s: Scope) {
        if self.deep > 0 || self.stack.len() >= MAX_DEPTH {
            self.deep += 1;
        } else {
            self.stack.push(s);
        }
    }

    /// 字符串外面的一个字节：数这一行开头是不是 `data:`
    fn at_line_start(&mut self, c: u8) {
        const DATA: &[u8] = b"data:";
        if c == b'\n' {
            self.line = Some(0);
            return;
        }
        let Some(n) = self.line else {
            return;
        };
        if DATA.get(n) == Some(&c) {
            self.line = Some(n + 1);
            if n + 1 == DATA.len() {
                self.stack.clear();
                self.deep = 0;
                self.line = None;
            }
        } else {
            self.line = None;
        }
    }

    /// 一个字符串开头的引号：模型名的值扣住，别的照发
    fn open_string(&mut self, out: &mut Vec<u8>) {
        self.in_str = true;
        self.escaped = false;
        let top = if self.deep > 0 {
            None
        } else {
            self.stack.last().copied()
        };
        self.capture = match top {
            Some(Scope::Obj {
                at: Level::Root | Level::Inner,
                want_key: true,
                ..
            }) => Capture::Key,
            Some(Scope::Obj {
                at: Level::Root | Level::Inner,
                key: Key::Model,
                want_key: false,
            }) => Capture::Value,
            _ => Capture::Skip,
        };
        match self.capture {
            Capture::Value => {
                self.held.clear();
                self.held.push(b'"');
            }
            Capture::Key => {
                self.key.clear();
                out.push(b'"');
            }
            Capture::Skip => out.push(b'"'),
        }
    }

    /// 字符串里面：从 `i` 读到这个字符串结束（或者这一段结束），返回读到了哪儿
    fn in_string(&mut self, chunk: &[u8], mut i: usize, out: &mut Vec<u8>) -> usize {
        while i < chunk.len() {
            if self.escaped {
                self.escaped = false;
                self.take(&chunk[i..=i], out);
                i += 1;
                continue;
            }
            // 一口气读到下一个要看的字节：字符串里绝大多数是正文，不必逐个判断
            let rest = &chunk[i..];
            let run = rest
                .iter()
                .position(|&b| matches!(b, b'"' | b'\\' | b'\n'))
                .unwrap_or(rest.len());
            self.take(&rest[..run], out);
            i += run;
            let Some(&c) = chunk.get(i) else {
                break;
            };
            i += 1;
            match c {
                b'\\' => {
                    self.escaped = true;
                    self.take(b"\\", out);
                }
                b'"' => {
                    self.in_str = false;
                    self.close_string(out);
                    return i;
                }
                // JSON 的字符串里不会有裸的换行：引号的配对已经乱了（帧被截断、不是 JSON）。
                // 扣着的原样交出去，从头数起，下一帧照样认得出
                _ => {
                    self.in_str = false;
                    self.capture = Capture::Skip;
                    out.append(&mut self.held);
                    out.push(b'\n');
                    self.stack.clear();
                    self.deep = 0;
                    self.line = Some(0);
                    return i;
                }
            }
        }
        i
    }

    /// 字符串里的一段：按正在读的是什么，发出去、记下、或者扣住
    fn take(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        match self.capture {
            Capture::Skip => out.extend_from_slice(bytes),
            Capture::Key => {
                out.extend_from_slice(bytes);
                // 认的几个键最长 13 个字节（`model_version`），多记几个就够分辨
                let room = 16usize.saturating_sub(self.key.len());
                self.key.extend_from_slice(&bytes[..bytes.len().min(room)]);
            }
            Capture::Value => {
                self.held.extend_from_slice(bytes);
                // 太长了不是模型名：原样放过去，这个字符串剩下的照发
                if self.held.len() > MAX_MODEL_LEN {
                    out.append(&mut self.held);
                    self.capture = Capture::Skip;
                }
            }
        }
    }

    fn close_string(&mut self, out: &mut Vec<u8>) {
        match std::mem::replace(&mut self.capture, Capture::Skip) {
            Capture::Skip => out.push(b'"'),
            Capture::Key => {
                out.push(b'"');
                let deep = self.deep;
                if let Some(Scope::Obj { at, key, .. }) =
                    self.stack.last_mut().filter(|_| deep == 0)
                {
                    *key = match (*at, self.key.as_slice()) {
                        (Level::Root, b"model" | b"modelVersion" | b"model_version") => Key::Model,
                        (Level::Root, b"message" | b"response") => Key::Wrapper,
                        (Level::Inner, b"model") => Key::Model,
                        _ => Key::Other,
                    };
                }
            }
            Capture::Value => {
                self.held.push(b'"');
                if self.renames_held() {
                    out.extend_from_slice(&self.quoted);
                    self.held.clear();
                } else {
                    out.append(&mut self.held);
                }
            }
        }
    }

    /// 扣住的这个名字（带着两头的引号）要不要换
    fn renames_held(&mut self) -> bool {
        if let Some((seen, wants)) = &self.last
            && *seen == self.held
        {
            return *wants;
        }
        // 带转义的（`\/`、`\u…`）照 JSON 的规矩解开；绝大多数名字没有
        let wants =
            serde_json::from_slice::<String>(&self.held).is_ok_and(|a| self.names.wants(&a));
        self.last = Some((self.held.clone(), wants));
        wants
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rename(client: &str, sent: &str) -> Body {
        Rename::new(client, sent).expect("names differ").body()
    }

    /// 整段一次喂进去，和逐字节喂进去，出来的必须一样
    fn through(b: &mut Body, input: &str) -> String {
        let mut out = b.feed(input.as_bytes());
        out.extend(b.flush());
        String::from_utf8(out).unwrap()
    }

    fn bytewise(client: &str, sent: &str, input: &str) -> String {
        let mut b = rename(client, sent);
        let mut out = Vec::new();
        for c in input.as_bytes() {
            out.extend(b.feed(std::slice::from_ref(c)));
        }
        out.extend(b.flush());
        String::from_utf8(out).unwrap()
    }

    fn check(client: &str, sent: &str, input: &str, want: &str) {
        assert_eq!(through(&mut rename(client, sent), input), want, "整段喂");
        assert_eq!(bytewise(client, sent, input), want, "逐字节喂");
    }

    #[test]
    fn the_same_names_need_no_rename() {
        assert!(Rename::new("claude-opus-5", "claude-opus-5").is_none());
        assert!(Rename::new("", "claude-opus-5").is_none());
        assert!(Rename::new("opus", "claude-opus-5").is_some());
    }

    #[test]
    fn an_anthropic_stream_renames_message_start_only() {
        let input = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-opus-5-20261001\",\"content\":[]}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"model\\\": \\\"claude-opus-5\\\"}\"}}\n\n",
        );
        let want = input.replace("\"claude-opus-5-20261001\"", "\"opus\"");
        check("opus", "us.anthropic.claude-opus-5-v1:0", input, &want);
    }

    #[test]
    fn an_anthropic_whole_body_renames_the_top_level_model_not_a_tool_input() {
        // 按键名排序的中转站：工具参数排在前面，里面也有一个 `model`
        let input = r#"{"content":[{"type":"tool_use","id":"t1","name":"pick","input":{"model":"claude-opus-5"}}],"id":"msg_1","model":"claude-opus-5","role":"assistant","type":"message"}"#;
        let want = r#"{"content":[{"type":"tool_use","id":"t1","name":"pick","input":{"model":"claude-opus-5"}}],"id":"msg_1","model":"opus","role":"assistant","type":"message"}"#;
        check("opus", "anthropic/claude-opus-5", input, want);
    }

    #[test]
    fn every_chat_chunk_is_renamed() {
        let input = concat!(
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-6-2026-09-01\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-6-2026-09-01\",\"choices\":[]}\n\n",
            "data: [DONE]\n\n",
        );
        let want = input.replace("gpt-6-2026-09-01", "my-gpt");
        check("my-gpt", "gpt-6", input, &want);
    }

    #[test]
    fn responses_events_rename_the_response_object() {
        let input = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"model\":\"gpt-6-codex\",\"output\":[]}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"arguments\":\"{}\",\"model\":\"gpt-6-codex\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"model\":\"gpt-6-codex\",\"usage\":{}}}\n\n",
        );
        let want = input.replace(
            "\"response\":{\"id\":\"r1\",\"model\":\"gpt-6-codex\"",
            "\"response\":{\"id\":\"r1\",\"model\":\"codex\"",
        );
        check("codex", "gpt-6-codex", input, &want);
        // 不是 `response` 下面的那一层不动
        assert!(want.contains(
            "\"item\":{\"type\":\"function_call\",\"arguments\":\"{}\",\"model\":\"gpt-6-codex\"}"
        ));
    }

    #[test]
    fn a_gemini_array_stream_renames_every_element() {
        let input = "[{\"candidates\":[],\"modelVersion\":\"gemini-3-pro-preview-09-2026\"}\r\n,{\"candidates\":[],\"modelVersion\":\"gemini-3-pro-preview-09-2026\"}]";
        let want = input.replace("gemini-3-pro-preview-09-2026", "gemini");
        check("gemini", "gemini-3-pro", input, &want);
    }

    #[test]
    fn a_different_model_is_left_alone() {
        let input = "data: {\"model\":\"gpt-6-mini\",\"choices\":[]}\n\n";
        check("my-gpt", "gpt-6", input, input);
    }

    #[test]
    fn an_empty_or_missing_model_is_not_filled_in() {
        let input = "data: {\"model\":\"\",\"choices\":[]}\n\ndata: {\"choices\":[]}\n\n";
        check("my-gpt", "gpt-6", input, input);
    }

    #[test]
    fn an_escaped_name_is_read_as_json() {
        let input = r#"{"model":"anthropic\/claude-opus-5"}"#;
        check("opus", "claude-opus-5", input, r#"{"model":"opus"}"#);
    }

    #[test]
    fn the_client_name_is_written_as_a_json_string() {
        let input = r#"{"model":"claude-opus-5"}"#;
        check(
            "中转\"别名\"",
            "claude-opus-5",
            input,
            r#"{"model":"中转\"别名\""}"#,
        );
    }

    #[test]
    fn an_overlong_value_passes_through() {
        let long = "x".repeat(MAX_MODEL_LEN + 10);
        let input = format!(r#"{{"model":"{long}","n":1}}"#);
        check("a", "b", &input, &input);
    }

    #[test]
    fn a_broken_frame_does_not_confuse_the_next() {
        let input = concat!(
            "data: {\"model\":\"gpt-6\",\"x\":\"cut off\n\n",
            "data: {\"model\":\"gpt-6\",\"choices\":[]}\n\n",
        );
        let want = concat!(
            "data: {\"model\":\"my-gpt\",\"x\":\"cut off\n\n",
            "data: {\"model\":\"my-gpt\",\"choices\":[]}\n\n",
        );
        check("my-gpt", "gpt-6", input, want);
    }

    #[test]
    fn a_name_cut_off_by_the_end_of_the_stream_is_handed_over_as_it_was() {
        let mut b = rename("my-gpt", "gpt-6");
        let mut out = b.feed(b"data: {\"model\":\"gpt-");
        out.extend(b.flush());
        assert_eq!(out, b"data: {\"model\":\"gpt-");
    }

    #[test]
    fn a_deeply_nested_answer_does_not_grow_the_stack() {
        let mut b = rename("my-gpt", "gpt-6");
        let mut out = b.feed(format!(r#"{{"a":{}"#, "[".repeat(10_000)).as_bytes());
        assert_eq!(b.stack.len(), MAX_DEPTH);
        out.extend(b.feed(format!(r#"{},"model":"gpt-6"}}"#, "]".repeat(10_000)).as_bytes()));
        out.extend(b.flush());
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.ends_with(r#"],"model":"my-gpt"}"#),
            "{}",
            &out[out.len() - 40..]
        );
    }

    #[test]
    fn headers_follow_the_same_rule() {
        let r = Rename::new("codex", "gpt-6-codex").unwrap();
        let mut h = HeaderMap::new();
        h.insert(
            "openai-model",
            HeaderValue::from_static("gpt-6-codex-2026-09-30"),
        );
        h.insert("x-openai-model", HeaderValue::from_static("gpt-6-codex"));
        r.headers(&mut h);
        assert_eq!(h["openai-model"], "codex");
        assert_eq!(h["x-openai-model"], "codex");
        // 真换了模型的照原样：Codex 要靠它提示
        let mut h = HeaderMap::new();
        h.insert("openai-model", HeaderValue::from_static("gpt-6-mini"));
        r.headers(&mut h);
        assert_eq!(h["openai-model"], "gpt-6-mini");
    }
}
