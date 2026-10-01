//! 从响应里嗅出 usage。
//!
//! **上游返回的 usage 是真相**，所以要拿到它。难点在于不能为此破坏
//! 出站直通：响应是流着出去的，**不能整块缓冲下来再解析** ——
//! 那会把 SSE 变成一次性交付，客户端看起来就是「卡很久然后一下全出来」。
//!
//! 所以这里是个**旁路嗅探器**：字节照常流向客户端，同时喂它一份。它只
//! 记那几个数字，内存是有界的。
//!
//! 五种格式、两种模式都要认：
//!
//! | | Anthropic | OpenAI Chat | OpenAI Responses | Gemini | Bedrock |
//! |---|---|---|---|---|---|
//! | 非流式 | 末尾一个 `usage` | 同 | 同 | `usageMetadata` | 末尾一个 `usage` |
//! | 流式 | `message_start` 给输入、`message_delta` 给输出 | 末尾一个带 `usage` 的 chunk | `response.completed` 里的 `usage` | 每一帧都带累计的 `usageMetadata` | `metadata` 事件里的 `usage` |
//!
//! Bedrock 的流是二进制帧，要由调用方先转成 SSE（`:event-type` 进 `event:`，
//! 载荷进 `data:`）再喂进来。
//!
//! 流式那一行决定了实现形状：usage **可能出现在流的任何位置，而且不止
//! 一次**，所以不能只看结尾。
//!
//! **数字怎么换算不在这里。**找到的对象交给那一家的 `usage()`（和方言转换用的
//! 是同一个），这里只负责「在字节流里找到它」和「认出它是哪一家的」。几家对
//! 「输入」的定义不一样（OpenAI 和 Gemini 的输入数包含缓存命中），那些换算
//! 各写在各家的模块里，只有一处。
//!
//! **同一遍里还认上游在回答里写的模型名**（[`Sniffer::model`]）：字节已经在这里
//! 过一遍了，事后再把响应体解析一遍只为一个名字，是白付一份钱。

use serde_json::Value;

pub use crate::ir::Usage;
use crate::{anthropic, bedrock, chat, gemini, responses};

/// 跨 chunk 的接续窗口。
///
/// 一个 usage 对象几百字节，而 chunk 边界可能正好切在中间。留这么多
/// 是为了让「上一块的尾巴 + 这一块」总能装下一个完整的 usage 对象。
const CARRY: usize = 4096;

/// 单次扫描里最多解析多长的一段。usage 对象是小东西，一个几 KB 的
/// 上限足以装下任何真实的，同时挡住「响应体里恰好有个叫 usage 的巨大
/// 字段」那种情况。
const MAX_OBJECT: usize = 8192;

/// 一边流一边嗅。
#[derive(Debug, Default)]
pub struct Sniffer {
    carry: Vec<u8>,
    seen: Usage,
    /// 总共喂进来多少字节。**超过一定量就不再嗅** —— 一个几十 MB 的
    /// 响应体里，usage 要么早就出现过，要么这家上游根本不给。
    fed: usize,
    /// 曾经解析出过至少一个 usage 对象
    found: bool,
    /// 回答里写的模型名
    model: ModelSpotter,
}

/// 超过这么多字节还没嗅到就放弃。放弃之后成本会走估算那条路，
/// 而估算值在界面上是标记过的。
const GIVE_UP_AFTER: usize = 32 * 1024 * 1024;

impl Sniffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一块。**这个函数在每个 chunk 上跑，所以它必须便宜** ——
    /// 先做一次朴素的子串查找，不命中就直接返回。
    pub fn feed(&mut self, chunk: &[u8]) {
        if self.fed > GIVE_UP_AFTER {
            return;
        }
        self.fed += chunk.len();
        self.model.feed(chunk);

        // 快速排除。**边界要单独看**：`"usage"` 七个字节完全可能被
        // chunk 切成两半，那时它在两边各自都找不到。这条是那个逐字节
        // 切开的测试当场抓住的 —— 漏掉它就是漏掉整次调用的成本。
        // 不带收尾引号：`"usage"` 和 Gemini 的 `"usageMetadata"` 都要命中
        const NEEDLE: &[u8] = b"\"usage";
        let edge = NEEDLE.len() - 1;
        let straddling = !self.carry.is_empty() && {
            let mut e = Vec::with_capacity(2 * edge);
            e.extend_from_slice(&self.carry[self.carry.len().saturating_sub(edge)..]);
            e.extend_from_slice(&chunk[..chunk.len().min(edge)]);
            memfind(&e, NEEDLE)
        };
        if !memfind(chunk, NEEDLE) && !memfind(&self.carry, NEEDLE) && !straddling {
            self.remember_tail(chunk);
            return;
        }

        let mut buf = Vec::with_capacity(self.carry.len() + chunk.len());
        buf.extend_from_slice(&self.carry);
        buf.extend_from_slice(chunk);
        self.scan(&buf);
        self.carry.clear();
        self.remember_tail(&buf);
    }

    fn remember_tail(&mut self, buf: &[u8]) {
        let start = buf.len().saturating_sub(CARRY);
        self.carry.clear();
        self.carry.extend_from_slice(&buf[start..]);
    }

    /// 找出这一段里所有完整的 usage 对象并合并。
    fn scan(&mut self, buf: &[u8]) {
        self.scan_key(buf, b"\"usage\"", false);
        self.scan_key(buf, b"\"usageMetadata\"", true);
    }

    fn scan_key(&mut self, buf: &[u8], key: &[u8], gemini: bool) {
        let mut from = 0;
        while let Some(i) = find_at(buf, key, from) {
            from = i + key.len();
            let Some(open) = buf[from..]
                .iter()
                .position(|c| !c.is_ascii_whitespace() && *c != b':')
            else {
                break;
            };
            let open = from + open;
            if buf.get(open) != Some(&b'{') {
                continue;
            }
            let Some(end) = match_braces(buf, open) else {
                // 对象还没收完 —— 留给下一块。**不要在这里猜**
                continue;
            };
            if let Ok(v) = serde_json::from_slice::<Value>(&buf[open..=end]) {
                self.merge(if gemini {
                    gemini::response::usage(&v)
                } else {
                    read(&v)
                });
            }
        }
    }

    /// 合并一个 usage 对象。
    ///
    /// **每个字段取较大值。**Anthropic 的 `message_start` 给输入、
    /// `message_delta` 给累计输出，两者都只带自己那部分；取较大值让
    /// 「后来的那个把前面的清零」不会发生。Gemini 每一帧都带累计数，
    /// 取较大值就是最终数。
    fn merge(&mut self, u: Usage) {
        if u.is_empty() {
            return;
        }
        self.found = true;
        self.seen.merge(&u);
    }

    /// 嗅到了什么。**没嗅到就是 None，不是零** —— 零会让一次真实的
    /// 调用看起来是免费的。
    pub fn finish(self) -> Option<Usage> {
        self.found.then_some(self.seen)
    }

    /// 上游在回答里写的模型名（见 [`ModelSpotter`]）。**没写就是 None** —— 不拿请求里
    /// 的名字去补：要比的正是这两个。
    pub fn model(&self) -> Option<&str> {
        self.model.found.as_deref()
    }
}

/// 最多看这么多帧（一帧一个根对象）：没有一帧写了模型名，就当这家不写，不再找。
///
/// 写模型名的几家都写在头一两帧里（Anthropic 的 `message_start`、Responses 的
/// `response.created`、Chat 和 Gemini 的每一帧；Azure 的头一帧是个模型名为空的
/// 内容审核帧）。不写的那家（Bedrock 的 Converse）就不该为它数完整条流。
const MODEL_FRAMES: u32 = 8;

/// 模型名最长多少字节。再长的不是模型名，不认
const MAX_MODEL_LEN: usize = 256;

/// JSON 最多数到第几层。再深就不数了：根对象和它下面那一层早就过去了，而一个故意
/// 嵌套很深的回答不该让这个栈一直长
const MAX_DEPTH: usize = 64;

/// 从回答里认出上游自己写的模型名。
///
/// **只认回答这一层的那个字段**，嵌在别处的同名键不算：
///
/// | | 在哪儿 |
/// |---|---|
/// | Anthropic | 整包的 `model`；流里 `message_start` 的 `message.model` |
/// | OpenAI Chat | 整包和每个 chunk 的 `model` |
/// | OpenAI Responses | 整包的 `model`；流里 `response.created` 这些帧的 `response.model` |
/// | Gemini | 整包和每一帧的 `modelVersion`（`model_version` 也认） |
/// | Bedrock | 不写：Converse 的回答里没有模型名 |
///
/// 「嵌在别处」的样子：Anthropic 整包里工具调用的 `input` 是一个原样的 JSON 对象，
/// 参数里完全可能有一个叫 `model` 的键 —— 而中转站重新序列化时可能按键名排序，把
/// `content` 排到 `model` 前面。认错了，就是在冤枉一家诚实的上游。
///
/// 所以边流边数 JSON 的层次，**不解析成值**：只在一帧的根对象、以及根下 `message` /
/// `response` 那个对象里认这几个键。帧是 SSE 的 `data:` 一行，或者整包那一个对象，
/// 或者 Gemini 不带 `alt=sse` 时那个数组里的每一个元素。认到一个非空的就停，之后的
/// 字节不再看。
#[derive(Debug, Default)]
struct ModelSpotter {
    found: Option<String>,
    /// 不再找了：认到了、看够了帧、或者层次深得不像一个回答
    done: bool,
    /// 看完了几个根对象
    roots: u32,
    stack: Vec<Scope>,
    in_str: bool,
    escaped: bool,
    /// 当前这个字符串攒不攒、攒来做什么
    capture: Capture,
    buf: Vec<u8>,
    /// 攒的字符串超长了
    overflow: bool,
    /// 这一行开头对上了 `data:` 的几个字节；不在行首、或者已经对不上了是 `None`。
    /// **对上了就是新的一帧**，层次从零数起：前一帧被截断、括号没配齐的话，后面的
    /// 每一帧都会被数成它里面的一层，再也认不出根对象
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
    /// 这一层认的模型名的键
    Model,
    /// 根下的 `message` / `response`：它的值是 [`Level::Inner`] 那一层
    Wrapper,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Capture {
    #[default]
    Skip,
    /// 一个键：根和 `Inner` 那两层的键要认是哪一个
    Key,
    /// 模型名的值
    Value,
}

impl ModelSpotter {
    fn feed(&mut self, chunk: &[u8]) {
        let mut i = 0;
        while i < chunk.len() && !self.done {
            if self.in_str {
                i = self.in_string(chunk, i);
                continue;
            }
            let c = chunk[i];
            i += 1;
            self.at_line_start(c);
            match c {
                b'"' => self.open_string(),
                b':' => {
                    if let Some(Scope::Obj { want_key, .. }) = self.stack.last_mut() {
                        *want_key = false;
                    }
                }
                b',' => {
                    if let Some(Scope::Obj { want_key, key, .. }) = self.stack.last_mut() {
                        *want_key = true;
                        *key = Key::None;
                    }
                }
                b'{' => {
                    let at = match self.stack.as_slice() {
                        [] | [Scope::Arr] => Level::Root,
                        [
                            ..,
                            Scope::Obj {
                                at: Level::Root,
                                key: Key::Wrapper,
                                want_key: false,
                            },
                        ] => Level::Inner,
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
                    if let Some(Scope::Obj {
                        at: Level::Root, ..
                    }) = self.stack.pop()
                    {
                        self.roots += 1;
                        self.done = self.roots >= MODEL_FRAMES;
                    }
                }
                _ => {}
            }
        }
    }

    fn push(&mut self, s: Scope) {
        if self.stack.len() >= MAX_DEPTH {
            self.done = true;
            return;
        }
        self.stack.push(s);
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
                self.line = None;
            }
        } else {
            self.line = None;
        }
    }

    fn open_string(&mut self) {
        self.in_str = true;
        self.escaped = false;
        self.overflow = false;
        self.buf.clear();
        self.capture = match self.stack.last() {
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
    }

    /// 字符串里面：从 `i` 读到这个字符串结束（或者这一块结束），返回读到了哪儿
    fn in_string(&mut self, chunk: &[u8], mut i: usize) -> usize {
        while i < chunk.len() {
            let c = chunk[i];
            i += 1;
            if self.escaped {
                self.escaped = false;
                self.keep(c);
                continue;
            }
            match c {
                b'\\' => {
                    self.escaped = true;
                    self.keep(c);
                }
                b'"' => {
                    self.in_str = false;
                    self.close_string();
                    return i;
                }
                // JSON 的字符串里不会有裸的换行：引号的配对已经乱了（帧被截断、
                // 不是 JSON）。从头数起，下一帧照样认得出
                b'\n' => {
                    self.in_str = false;
                    self.stack.clear();
                    self.line = Some(0);
                    return i;
                }
                _ => self.keep(c),
            }
        }
        i
    }

    fn keep(&mut self, c: u8) {
        let limit = match self.capture {
            Capture::Skip => return,
            // 认的几个键最长 13 个字节（`model_version`），再长就不是它们
            Capture::Key => 16,
            Capture::Value => MAX_MODEL_LEN,
        };
        if self.buf.len() >= limit {
            self.overflow = true;
        } else {
            self.buf.push(c);
        }
    }

    fn close_string(&mut self) {
        match std::mem::take(&mut self.capture) {
            Capture::Skip => {}
            Capture::Key => {
                if let Some(Scope::Obj { at, key, .. }) = self.stack.last_mut() {
                    *key = match (*at, self.overflow, self.buf.as_slice()) {
                        (_, true, _) => Key::Other,
                        (Level::Root, _, b"model" | b"modelVersion" | b"model_version") => {
                            Key::Model
                        }
                        (Level::Root, _, b"message" | b"response") => Key::Wrapper,
                        (Level::Inner, _, b"model") => Key::Model,
                        _ => Key::Other,
                    };
                }
            }
            Capture::Value => {
                if self.overflow {
                    return;
                }
                // 带转义的（`\/`、`\u…`）照 JSON 的规矩解开；绝大多数名字没有
                let name = if self.buf.contains(&b'\\') {
                    let mut quoted = Vec::with_capacity(self.buf.len() + 2);
                    quoted.push(b'"');
                    quoted.extend_from_slice(&self.buf);
                    quoted.push(b'"');
                    serde_json::from_slice::<String>(&quoted).ok()
                } else {
                    String::from_utf8(self.buf.clone()).ok()
                };
                // 空的不算：Azure 头一帧的 `"model": ""` 说的是「还没开始」
                if let Some(name) = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()) {
                    self.found = Some(name);
                    self.done = true;
                }
            }
        }
    }
}

/// 读一个不知道是哪家的 `usage` 对象：按只有那一家才有的字段名认出来，
/// 交给那一家的解析器。
///
/// Anthropic 和 Responses 都叫 `input_tokens` / `output_tokens`，区别在明细：
/// Responses 有 `input_tokens_details` 和 `total_tokens`，而且它的输入数包含
/// 缓存命中 —— 认错了，缓存那部分会按输入价再算一遍。
pub fn read(v: &Value) -> Usage {
    let has = |k: &str| v.get(k).is_some();
    if has("prompt_tokens") || has("completion_tokens") || has("prompt_tokens_details") {
        chat::response::usage(v)
    } else if has("inputTokens")
        || has("outputTokens")
        || has("cacheReadInputTokens")
        || has("cacheWriteInputTokens")
    {
        bedrock::response::usage(v)
    } else if has("input_tokens_details") || has("total_tokens") {
        responses::response::usage(v)
    } else {
        anthropic::response::usage(v)
    }
}

fn memfind(hay: &[u8], needle: &[u8]) -> bool {
    find_at(hay, needle, 0).is_some()
}

fn find_at(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() || needle.len() > hay.len() - from {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

/// 从 `open`（一个 `{`）开始配对，返回对应 `}` 的下标。
///
/// **要认字符串里的花括号。**`{"note":"}"}`  会让一个朴素的计数器提前
/// 收尾，然后解析出一个残缺的对象。
fn match_braces(buf: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &c) in buf.iter().enumerate().skip(open) {
        if i - open > MAX_OBJECT {
            return None;
        }
        if in_str {
            match c {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sniff(chunks: &[&str]) -> Option<Usage> {
        let mut s = Sniffer::new();
        for c in chunks {
            s.feed(c.as_bytes());
        }
        s.finish()
    }

    #[test]
    fn a_responses_stream_subtracts_its_cached_tokens() {
        let u = sniff(&[
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"usage\":null}}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5000,\"input_tokens_details\":{\"cached_tokens\":4000,\"cache_write_tokens\":0},\"output_tokens\":300,\"output_tokens_details\":{\"reasoning_tokens\":200},\"total_tokens\":5300}}}\n\n",
        ])
        .unwrap();
        assert_eq!((u.input, u.cache_read, u.output), (1000, 4000, 300));
    }

    #[test]
    fn a_gemini_stream_is_read_from_usage_metadata() {
        // 以前只认 `"usage"`，Gemini 的请求一个数字都记不到
        let chunk = |candidates: u64| {
            format!(
                "data: {{\"candidates\":[],\"usageMetadata\":{{\"promptTokenCount\":1000,\"cachedContentTokenCount\":600,\"candidatesTokenCount\":{candidates},\"thoughtsTokenCount\":50}}}}\r\n\r\n"
            )
        };
        let (a, b) = (chunk(10), chunk(80));
        let u = sniff(&[&a, &b]).unwrap();
        assert_eq!((u.input, u.cache_read, u.output), (400, 600, 130));
    }

    #[test]
    fn a_bedrock_response_is_read_by_its_own_names() {
        // Converse 用驼峰，而且和 Anthropic 一样：`inputTokens` 不含缓存。
        // 以前一个都不认，Bedrock 的请求全按估算记账
        let whole = r#"{"output":{"message":{"role":"assistant","content":[{"text":"hi"}]}},"stopReason":"end_turn",
            "usage":{"inputTokens":60,"outputTokens":20,"cacheReadInputTokens":40,"cacheWriteInputTokens":5,"totalTokens":125}}"#;
        let u = sniff(&[whole]).unwrap();
        assert_eq!(
            (u.input, u.cache_read, u.cache_write, u.output),
            (60, 40, 5, 20)
        );

        // 流：拆帧之后是 `metadata` 事件
        let u = sniff(&[
            "event: messageStop\ndata: {\"stopReason\":\"end_turn\"}\n\n",
            "event: metadata\ndata: {\"usage\":{\"inputTokens\":60,\"outputTokens\":20,\"totalTokens\":80},\"metrics\":{\"latencyMs\":300}}\n\n",
        ])
        .unwrap();
        assert_eq!((u.input, u.output), (60, 20));
    }

    #[test]
    fn a_needle_cut_between_chunks_still_finds_usage_metadata() {
        let whole = r#"{"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5}}"#;
        for cut in 1..whole.len() {
            let u = sniff(&[&whole[..cut], &whole[cut..]]);
            assert_eq!(
                u.map(|u| (u.input, u.output)),
                Some((10, 5)),
                "在第 {cut} 字节切开"
            );
        }
    }

    #[test]
    fn an_anthropic_non_streaming_response_gives_all_four_numbers() {
        let body = r#"{"id":"msg_01","type":"message","content":[{"type":"text","text":"hi"}],
            "usage":{"input_tokens":1200,"output_tokens":340,
                     "cache_read_input_tokens":800,"cache_creation_input_tokens":100}}"#;
        let u = sniff(&[body]).unwrap();
        assert_eq!(u.input, 1200);
        assert_eq!(u.output, 340);
        assert_eq!(u.cache_read, 800);
        assert_eq!(u.cache_write, 100);
    }

    #[test]
    fn an_anthropic_stream_merges_message_start_and_message_delta() {
        // **输入在开头、输出在结尾**，中间隔着整个回答。只看结尾会漏掉
        // 输入 token，而那通常是账单里最大的一块。
        let u = sniff(&[
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5000,\"output_tokens\":1,\"cache_read_input_tokens\":4000}}}\n\n",
            "event: content_block_delta\ndata: {\"delta\":{\"text\":\"一段很长的回答\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":777}}\n\n",
        ])
        .unwrap();
        assert_eq!(u.input, 5000);
        assert_eq!(u.output, 777, "结尾那个累计输出没盖住开头那个 1");
        assert_eq!(u.cache_read, 4000);
    }

    #[test]
    fn a_usage_object_split_across_two_chunks_is_still_read() {
        // **chunk 边界会切在任何地方。**切断了就漏掉整次调用的成本。
        let full = r#"{"content":[],"usage":{"input_tokens":123,"output_tokens":45}}"#;
        for cut in 1..full.len() {
            let (a, b) = full.split_at(cut);
            let u = sniff(&[a, b]).unwrap_or_else(|| panic!("在第 {cut} 个字节切开就嗅不到了"));
            assert_eq!((u.input, u.output), (123, 45), "cut={cut}");
        }
    }

    #[test]
    fn an_openai_response_uses_its_own_field_names() {
        let u = sniff(&[
            r#"{"choices":[],"usage":{"prompt_tokens":900,"completion_tokens":120,"total_tokens":1020}}"#,
        ])
        .unwrap();
        assert_eq!(u.input, 900);
        assert_eq!(u.output, 120);
    }

    #[test]
    fn openai_cached_tokens_are_subtracted_from_the_prompt_because_they_overlap() {
        // **OpenAI 的 `cached_tokens` 是 `prompt_tokens` 的子集。**
        // 直接相加会把缓存那部分算两遍，而缓存读通常是最大的一块 ——
        // 那个错会让账单看起来高得离谱。
        let u = sniff(&[r#"{"usage":{"prompt_tokens":10000,"completion_tokens":50,
                "prompt_tokens_details":{"cached_tokens":9000}}}"#])
        .unwrap();
        assert_eq!(u.cache_read, 9000);
        assert_eq!(u.input, 1000, "没缓存的那部分该是 10000 - 9000");
    }

    #[test]
    fn the_one_hour_cache_tier_is_picked_up_because_it_costs_almost_double() {
        // Sonnet 4.5 是 5 分钟 $3.75、1 小时 $6.00。认不出来
        // 就会系统性低估 60%。
        let u = sniff(&[
            r#"{"usage":{"input_tokens":10,"cache_creation_input_tokens":2000,
                "cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":2000}}}"#,
        ])
        .unwrap();
        assert!(u.cache_1h, "1 小时档没认出来");
        assert_eq!(u.cache_write, 2000);
    }

    #[test]
    fn a_five_minute_cache_write_is_not_marked_as_one_hour() {
        let u = sniff(&[
            r#"{"usage":{"cache_creation_input_tokens":2000,
                "cache_creation":{"ephemeral_5m_input_tokens":2000,"ephemeral_1h_input_tokens":0}}}"#,
        ])
        .unwrap();
        assert!(!u.cache_1h);
        assert_eq!(u.cache_write, 2000);
    }

    #[test]
    fn a_response_without_usage_yields_none_not_zero() {
        // **零会让一次真实的调用看起来是免费的**。有些中转站
        // 就是不给 usage，那时该走估算那条路，而不是记一笔 0。
        assert!(sniff(&[r#"{"id":"msg_01","content":[{"type":"text","text":"hi"}]}"#]).is_none());
        assert!(sniff(&[""]).is_none());
        assert!(sniff(&["event: ping\ndata: {}\n\n"]).is_none());
    }

    #[test]
    fn a_brace_inside_a_string_does_not_end_the_object_early() {
        // `{"note":"}"}` 会让一个朴素的计数器提前收尾，然后解析出一个
        // 残缺的对象 —— 而那次调用的成本就没了。
        let u = sniff(&[
            r#"{"usage":{"note":"a } and a \" quote","input_tokens":7,"output_tokens":8}}"#,
        ])
        .unwrap();
        assert_eq!((u.input, u.output), (7, 8));
    }

    #[test]
    fn the_word_usage_appearing_in_the_content_does_not_confuse_it() {
        // 用户完全可能在问「usage 是什么意思」。
        let u = sniff(&[
            r#"{"content":[{"type":"text","text":"\"usage\" 这个词的意思是用量"}],"usage":{"input_tokens":3,"output_tokens":4}}"#,
        ])
        .unwrap();
        assert_eq!((u.input, u.output), (3, 4));
    }

    #[test]
    fn a_huge_body_without_usage_does_not_grow_memory() {
        // **这个嗅探器跑在每个 chunk 上。**它要是攒着不放，一个长会话
        // 就能把内存吃光。
        let mut s = Sniffer::new();
        let junk = vec![b'x'; 64 * 1024];
        for _ in 0..64 {
            s.feed(&junk);
        }
        assert!(s.carry.len() <= CARRY, "接续窗口涨到了 {}", s.carry.len());
        assert!(s.finish().is_none());
    }

    #[test]
    fn usage_arriving_late_in_a_long_stream_is_still_caught() {
        let mut s = Sniffer::new();
        for _ in 0..200 {
            s.feed(b"event: content_block_delta\ndata: {\"delta\":{\"text\":\"...\"}}\n\n");
        }
        s.feed(br#"data: {"usage":{"input_tokens":11,"output_tokens":22}}"#);
        let u = s.finish().unwrap();
        assert_eq!((u.input, u.output), (11, 22));
    }

    #[test]
    fn chat_cache_writes_are_also_part_of_the_prompt() {
        let u = sniff(&[r#"{"usage":{"prompt_tokens":1000,"completion_tokens":50,
                "prompt_tokens_details":{"cached_tokens":600,"cache_write_tokens":300},
                "completion_tokens_details":{"reasoning_tokens":20}}}"#])
        .unwrap();
        assert_eq!(
            (u.input, u.cache_read, u.cache_write, u.output, u.reasoning),
            (100, 600, 300, 50, 20)
        );
    }

    #[test]
    fn the_sniffer_and_the_dialect_read_a_usage_object_the_same_way() {
        // 同一个对象，旁路嗅探和方言转换得出同一组数 —— 换算只有一处
        let anthropic = serde_json::json!({"input_tokens":10,"cache_creation_input_tokens":2000,
            "cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":2000}});
        let responses = serde_json::json!({"input_tokens":5000,"output_tokens":300,"total_tokens":5300,
            "input_tokens_details":{"cached_tokens":4000}});
        let bedrock =
            serde_json::json!({"inputTokens":60,"outputTokens":20,"cacheReadInputTokens":40});
        for (v, parsed) in [
            (&anthropic, anthropic::response::usage(&anthropic)),
            (&responses, responses::response::usage(&responses)),
            (&bedrock, bedrock::response::usage(&bedrock)),
        ] {
            assert_eq!(read(v), parsed, "{v}");
            let sniffed = sniff(&[&format!(r#"{{"usage":{v}}}"#)]).unwrap();
            assert_eq!(sniffed, parsed, "{v}");
        }
        assert!(read(&anthropic).cache_1h);
    }

    #[test]
    fn an_incomplete_usage_object_at_the_very_end_is_not_half_parsed() {
        // 上游中途断了。**半个 usage 比没有 usage 危险** —— 它会变成一个
        // 看起来正常但偏低的数字。
        assert!(sniff(&[r#"{"usage":{"input_tokens":123,"output_"#]).is_none());
    }
}

#[cfg(test)]
mod model_tests {
    use super::*;

    /// 一整段喂进去，再一个字节一个字节地喂一遍：**两种喂法认出的必须是同一个**
    /// —— chunk 的边界会切在任何地方，包括键名和模型名的中间。
    fn model(body: &str) -> Option<String> {
        let mut whole = Sniffer::new();
        whole.feed(body.as_bytes());
        let mut bytes = Sniffer::new();
        for b in body.as_bytes() {
            bytes.feed(std::slice::from_ref(b));
        }
        assert_eq!(whole.model(), bytes.model(), "逐字节喂的结果不一样：{body}");
        whole.model().map(str::to_string)
    }

    #[test]
    fn an_anthropic_stream_names_its_model_in_message_start() {
        let body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",",
            "\"role\":\"assistant\",\"model\":\"claude-sonnet-4-5-20250929\",\"content\":[],",
            "\"usage\":{\"input_tokens\":12,\"output_tokens\":1}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
        );
        assert_eq!(model(body).as_deref(), Some("claude-sonnet-4-5-20250929"));
    }

    #[test]
    fn an_anthropic_response_names_its_model_at_the_top() {
        let body = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-4-1-20250805",
            "content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":3,"output_tokens":1}}"#;
        assert_eq!(model(body).as_deref(), Some("claude-opus-4-1-20250805"));
    }

    /// **工具参数里的 `model` 不是回答的模型名。**按键名排序的中转站会把 `content`
    /// 排到 `model` 前面，参数先到 —— 认成它，就是冤枉一家诚实的上游。
    #[test]
    fn a_model_key_inside_a_tool_call_is_not_the_answer() {
        let sorted = r#"{"content":[{"id":"t1","input":{"model":"gpt-4o-mini","n":1},"name":"pick","type":"tool_use"}],
            "id":"msg_1","model":"claude-sonnet-4-5-20250929","role":"assistant","type":"message"}"#;
        assert_eq!(model(sorted).as_deref(), Some("claude-sonnet-4-5-20250929"));
        // 回答这一层没写的：嵌在里面的那个照样不算
        let nested_only =
            r#"{"content":[{"type":"tool_use","input":{"model":"gpt-4o-mini"}}],"type":"message"}"#;
        assert_eq!(model(nested_only), None);
        // 流里工具参数是转义过的字符串，本来就认不错
        let stream = concat!(
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",",
            "\"partial_json\":\"{\\\"model\\\": \\\"x\\\"}\"}}\n\n",
        );
        assert_eq!(model(stream), None);
    }

    #[test]
    fn a_chat_stream_names_its_model_in_every_chunk() {
        let body = concat!(
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-2024-08-06\",",
            "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-2024-08-06\",",
            "\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":1}}\n\n",
            "data: [DONE]\n\n",
        );
        assert_eq!(model(body).as_deref(), Some("gpt-4o-2024-08-06"));
    }

    /// Azure 的头一帧是内容审核的结果，模型名是空的：**空的不算**，往下一帧找
    #[test]
    fn an_empty_model_in_the_first_chunk_is_skipped() {
        let body = concat!(
            "data: {\"choices\":[],\"created\":0,\"id\":\"\",\"model\":\"\",\"object\":\"\",",
            "\"prompt_filter_results\":[{\"prompt_index\":0,\"content_filter_results\":{}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"index\":0}],\"created\":1,",
            "\"id\":\"c1\",\"model\":\"gpt-4o-2024-11-20\",\"object\":\"chat.completion.chunk\"}\n\n",
        );
        assert_eq!(model(body).as_deref(), Some("gpt-4o-2024-11-20"));
    }

    #[test]
    fn a_chat_response_names_its_model_at_the_top() {
        let body = r#"{"id":"c1","object":"chat.completion","created":1,"model":"deepseek-chat",
            "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6}}"#;
        assert_eq!(model(body).as_deref(), Some("deepseek-chat"));
    }

    #[test]
    fn a_responses_stream_names_its_model_in_the_response_it_opens_with() {
        let body = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",",
            "\"object\":\"response\",\"model\":\"gpt-5.1-codex\",\"output\":[],\"usage\":null}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
        );
        assert_eq!(model(body).as_deref(), Some("gpt-5.1-codex"));
    }

    #[test]
    fn a_responses_response_names_its_model_at_the_top() {
        let body = r#"{"id":"resp_1","object":"response","created_at":1,"status":"completed",
            "model":"gpt-5-2025-08-07","output":[{"type":"message","content":[{"type":"output_text","text":"hi"}]}],
            "usage":{"input_tokens":5,"output_tokens":1,"total_tokens":6}}"#;
        assert_eq!(model(body).as_deref(), Some("gpt-5-2025-08-07"));
    }

    /// Gemini 的模型名在每一帧的末尾，在候选内容之后 —— 内容里的 `"role":"model"`
    /// 是一个值，不是模型名
    #[test]
    fn a_gemini_stream_names_its_model_version_after_the_candidates() {
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"hi\"}]}}],",
            "\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":1},",
            "\"modelVersion\":\"gemini-2.5-pro\",\"responseId\":\"r1\"}\r\n\r\n",
        );
        assert_eq!(model(body).as_deref(), Some("gemini-2.5-pro"));
        // 下划线的写法也认（和 Gemini 那一家的解析器一样）
        let snake = r#"{"candidates":[],"model_version":"gemini-2.5-flash"}"#;
        assert_eq!(model(snake).as_deref(), Some("gemini-2.5-flash"));
    }

    /// 不带 `alt=sse` 的 Gemini 流是一个逐步写出的 JSON 数组：数组里的每个元素是一帧
    #[test]
    fn a_gemini_json_array_stream_names_its_model_in_each_element() {
        let body = "[{\n  \"candidates\": [{\"content\": {\"role\": \"model\", \"parts\": [{\"text\": \"hi\"}]}}],\n  \"modelVersion\": \"gemini-2.5-flash\"\n}\r\n,\r\n{\n  \"candidates\": []\n}\r\n]";
        assert_eq!(model(body).as_deref(), Some("gemini-2.5-flash"));
    }

    /// Bedrock 的 Converse 不写模型名：**没有就是没有**，看够几帧就不再找
    #[test]
    fn a_bedrock_stream_names_no_model_and_the_search_stops() {
        let mut s = Sniffer::new();
        for _ in 0..20 {
            s.feed(b"event: contentBlockDelta\ndata: {\"contentBlockIndex\":0,\"delta\":{\"text\":\"hi\"}}\n\n");
        }
        s.feed(b"event: metadata\ndata: {\"usage\":{\"inputTokens\":3,\"outputTokens\":1}}\n\n");
        assert_eq!(s.model(), None);
        assert!(s.model.done, "看够了帧还在找");
        // 用量照常
        assert_eq!(s.finish().map(|u| u.input), Some(3));
    }

    #[test]
    fn an_escaped_model_name_is_unescaped() {
        let body = r#"{"model":"models\/gemini-2.5-pro","candidates":[]}"#;
        assert_eq!(model(body).as_deref(), Some("models/gemini-2.5-pro"));
    }

    /// 前一帧被截断了（括号没配齐、引号没收）：**下一帧照样认得出**
    #[test]
    fn a_broken_frame_does_not_hide_the_next_one() {
        let unclosed = concat!(
            "data: {\"type\":\"ping\",\"x\":{\"y\":1\n\n",
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-haiku-4-5\"}}\n\n",
        );
        assert_eq!(model(unclosed).as_deref(), Some("claude-haiku-4-5"));
        let cut_in_a_string = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"mod\n\n",
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-haiku-4-5\"}}\n\n",
        );
        assert_eq!(model(cut_in_a_string).as_deref(), Some("claude-haiku-4-5"));
    }

    /// 只认根和 `message` / `response` 那一层：别的键下面的对象里写的 `model` 不算
    #[test]
    fn only_the_answer_level_counts() {
        assert_eq!(model(r#"{"meta":{"model":"x"}}"#), None);
        assert_eq!(
            model(r#"{"message":{"content":[{"model":"x"}]},"type":"message_start"}"#),
            None
        );
        // 值不是字符串的不算
        assert_eq!(model(r#"{"model":null,"x":1}"#), None);
        assert_eq!(model(r#"{"model":{"id":"x"}}"#), None);
        // 错误响应里的 `"param":"model"` 是一个值
        assert_eq!(
            model(r#"{"error":{"message":"bad","param":"model","code":"model_not_found"}}"#),
            None
        );
    }

    #[test]
    fn a_response_without_a_model_has_none_and_usage_is_unaffected() {
        let mut s = Sniffer::new();
        s.feed(br#"{"id":"msg_01","content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":7,"output_tokens":8}}"#);
        assert_eq!(s.model(), None);
        assert_eq!(s.finish().map(|u| (u.input, u.output)), Some((7, 8)));
    }

    /// 嵌得很深的回答：数到上限就不数了，不 panic、不一直长
    #[test]
    fn deep_nesting_stops_the_search() {
        let mut s = Sniffer::new();
        s.feed(&b"[".repeat(10_000));
        assert!(s.model.stack.len() <= MAX_DEPTH);
        assert!(s.model.done);
        assert_eq!(s.model(), None);
    }

    /// 超长的值不是模型名
    #[test]
    fn an_overlong_value_is_not_a_model_name() {
        let body = format!(r#"{{"model":"{}"}}"#, "x".repeat(MAX_MODEL_LEN + 1));
        assert_eq!(model(&body), None);
    }
}
