//! 模型把工具调用写进了回答正文：认出来，换成结构化的调用。
//!
//! 说 Chat 格式的上游不止 OpenAI：DeepSeek、GLM、Qwen、Llama 这些模型（尤其是自己部署
//! 的、或者经中转站转发的）有时不走 `tool_calls`，而是把调用按训练时的模板直接写进
//! `content`。客户端看到的是一段带标签的文字，什么都不会执行；工具调用审查也看不到它。
//!
//! 认四种写法：
//!
//! 1. `<tool_call>{"name": …, "arguments": {…}}</tool_call>`（Qwen、Hermes；参数也可以叫
//!    `parameters`）
//! 2. DeepSeek：`<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>function<｜tool▁sep｜>名字\n`
//!    ```` ```json\n{…}\n``` ````` `<｜tool▁call▁end｜>…<｜tool▁calls▁end｜>`，也认不写
//!    `function`、不带代码围栏的新写法
//! 3. GLM：`<tool_call>名字\n<arg_key>k</arg_key>\n<arg_value>v</arg_value>…</tool_call>`
//! 4. `<function=名字><parameter name="k">v</parameter>…</function>`（Llama 一系）
//!
//! **只认请求里定义过的工具名**：名字对不上的原样留在正文里。**只认带标签的写法**：
//! 正文里一段裸的 JSON 对象不碰，模型解释代码时写的 JSON 太多了。收尾标签在回答末尾
//! 缺了也认（模型到上限被截断、或者模板里本来就不写）。
//!
//! 只在上游说 Chat 格式、请求带了工具定义、回答里没有结构化的 `tool_calls` 时做。
//! 同格式直通不经过这里。

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use crate::ir::*;

const TAG_OPEN: &str = "<tool_call>";
const TAG_CLOSE: &str = "</tool_call>";
const DS_CALLS_BEGIN: &str = "<｜tool▁calls▁begin｜>";
const DS_CALLS_END: &str = "<｜tool▁calls▁end｜>";
const DS_CALL_BEGIN: &str = "<｜tool▁call▁begin｜>";
const DS_CALL_END: &str = "<｜tool▁call▁end｜>";
const DS_SEP: &str = "<｜tool▁sep｜>";
const FN_OPEN: &str = "<function=";
const FN_CLOSE: &str = "</function>";

/// 正文拆开后的一段。
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    Text(String),
    Call(Box<ToolCall>),
}

/// 哪种写法开了头。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    /// `<tool_call>`：JSON 或 GLM 的键值对
    Tag,
    /// DeepSeek 的整组调用
    DeepSeekGroup,
    /// DeepSeek 的单个调用，没有外层标记
    DeepSeekOne,
    /// `<function=名字>`
    Function,
}

impl Form {
    fn open(self) -> &'static str {
        match self {
            Form::Tag => TAG_OPEN,
            Form::DeepSeekGroup => DS_CALLS_BEGIN,
            Form::DeepSeekOne => DS_CALL_BEGIN,
            Form::Function => FN_OPEN,
        }
    }

    /// 流里从开头标记起要攒到哪个标记为止
    fn close(self) -> &'static str {
        match self {
            Form::Tag => TAG_CLOSE,
            Form::DeepSeekGroup => DS_CALLS_END,
            Form::DeepSeekOne => DS_CALL_END,
            Form::Function => FN_CLOSE,
        }
    }
}

const FORMS: [Form; 4] = [
    Form::Tag,
    Form::DeepSeekGroup,
    Form::DeepSeekOne,
    Form::Function,
];

/// 最早出现的开头标记：在哪、是哪种
fn next_marker(s: &str) -> Option<(usize, Form)> {
    FORMS
        .iter()
        .filter_map(|f| s.find(f.open()).map(|i| (i, *f)))
        .min_by_key(|(i, f)| (*i, std::cmp::Reverse(f.open().len())))
}

/// 一段正文 → 文字和调用。**纯函数**：名字对不上、解析不了的原样留作文字。
pub fn split(text: &str, tools: &HashSet<String>) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::new();
    let mut pos = 0;
    while let Some((at, form)) = next_marker(&text[pos..]) {
        let at = pos + at;
        push_text(&mut out, &text[pos..at]);
        match parse_block(&text[at..], form, tools) {
            Some((pieces, consumed)) => {
                for p in pieces {
                    match p {
                        Piece::Text(t) => push_text(&mut out, &t),
                        call => out.push(call),
                    }
                }
                pos = at + consumed;
            }
            // 不是调用：标记本身留作文字，从它后面接着找 —— 它后面可能还有真的
            None => {
                let len = form.open().len();
                push_text(&mut out, &text[at..at + len]);
                pos = at + len;
            }
        }
    }
    push_text(&mut out, &text[pos..]);
    // 换成了调用的话，调用之间只剩空白的文字不要：客户端会把它显示成一段空话
    if out.iter().any(|p| matches!(p, Piece::Call(_))) {
        out.retain(|p| !matches!(p, Piece::Text(t) if t.trim().is_empty()));
    }
    out
}

fn push_text(out: &mut Vec<Piece>, t: &str) {
    if t.is_empty() {
        return;
    }
    match out.last_mut() {
        Some(Piece::Text(prev)) => prev.push_str(t),
        _ => out.push(Piece::Text(t.to_string())),
    }
}

/// `rest` 以某种开头标记起头：解出这一块里的调用，和这一块有多长。
/// 一个调用都认不出的是 None。
fn parse_block(rest: &str, form: Form, tools: &HashSet<String>) -> Option<(Vec<Piece>, usize)> {
    match form {
        Form::Tag => {
            let body_at = TAG_OPEN.len();
            let (inner, consumed) = match rest[body_at..].find(TAG_CLOSE) {
                Some(i) => (&rest[body_at..body_at + i], body_at + i + TAG_CLOSE.len()),
                None => (&rest[body_at..], rest.len()),
            };
            let inner = inner.trim();
            let call = if inner.starts_with('{') {
                json_call(inner)
            } else {
                glm_call(inner)
            };
            let call = call.filter(|c| tools.contains(&c.name))?;
            Some((vec![Piece::Call(Box::new(call))], consumed))
        }
        Form::DeepSeekGroup | Form::DeepSeekOne => deepseek_block(rest, form, tools),
        Form::Function => {
            let after = &rest[FN_OPEN.len()..];
            let gt = after.find('>')?;
            let name = after[..gt].trim();
            if !plausible_name(name) || !tools.contains(name) {
                return None;
            }
            let body_at = FN_OPEN.len() + gt + 1;
            let (body, consumed) = match rest[body_at..].find(FN_CLOSE) {
                Some(i) => (&rest[body_at..body_at + i], body_at + i + FN_CLOSE.len()),
                None => (&rest[body_at..], rest.len()),
            };
            let input = if body.trim().starts_with('{') {
                serde_json::from_str::<Value>(body.trim()).ok()?
            } else {
                Value::Object(parameters(body))
            };
            Some((
                vec![Piece::Call(Box::new(ToolCall {
                    id: new_id("call_"),
                    name: name.to_string(),
                    input: ToolInput::Json(input),
                }))],
                consumed,
            ))
        }
    }
}

/// `{"name": …, "arguments": {…}}`。参数写成字符串的（里面是 JSON 文本）也认
fn json_call(inner: &str) -> Option<ToolCall> {
    let v: Value = serde_json::from_str(inner).ok()?;
    let name = v.get("name")?.as_str()?;
    if !plausible_name(name) {
        return None;
    }
    let input = match v.get("arguments").or_else(|| v.get("parameters")) {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(Value::String(s)) => {
            serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone()))
        }
        Some(x) => x.clone(),
    };
    Some(ToolCall {
        id: new_id("call_"),
        name: name.to_string(),
        input: ToolInput::Json(input),
    })
}

/// GLM：第一行是名字，后面是 `<arg_key>k</arg_key><arg_value>v</arg_value>` 对
fn glm_call(inner: &str) -> Option<ToolCall> {
    let (name, rest) = inner.split_once('\n').unwrap_or((inner, ""));
    let name = name.trim();
    if !plausible_name(name) {
        return None;
    }
    let mut args = Map::new();
    let mut s = rest;
    while let Some(k) = s.find("<arg_key>") {
        let s1 = &s[k + "<arg_key>".len()..];
        let Some(ke) = s1.find("</arg_key>") else {
            break;
        };
        let key = s1[..ke].trim();
        let s2 = &s1[ke + "</arg_key>".len()..];
        let Some(v) = s2.find("<arg_value>") else {
            break;
        };
        let s3 = &s2[v + "<arg_value>".len()..];
        // 收尾标签缺了（回答在这里截断）：到末尾为止都是值
        let (value, after) = match s3.find("</arg_value>") {
            Some(ve) => (&s3[..ve], &s3[ve + "</arg_value>".len()..]),
            None => (s3, ""),
        };
        args.insert(key.to_string(), typed(value));
        s = after;
    }
    Some(ToolCall {
        id: new_id("call_"),
        name: name.to_string(),
        input: ToolInput::Json(Value::Object(args)),
    })
}

/// `<parameter name="k">v</parameter>` 对（也认 `<parameter=k>v</parameter>`，那种写法
/// 值前后带换行，去掉）。最后一个的收尾标签缺了也认
fn parameters(body: &str) -> Map<String, Value> {
    let mut args = Map::new();
    let mut s = body;
    while let Some(p) = s.find("<parameter") {
        let s1 = &s[p + "<parameter".len()..];
        let (key, s2, trim) = if let Some(q) = s1.strip_prefix(" name=\"") {
            let Some(qe) = q.find('"') else { break };
            let Some(gt) = q[qe..].find('>') else { break };
            (&q[..qe], &q[qe + gt + 1..], false)
        } else if let Some(q) = s1.strip_prefix('=') {
            let Some(gt) = q.find('>') else { break };
            (q[..gt].trim(), &q[gt + 1..], true)
        } else {
            break;
        };
        let (value, after) = match s2.find("</parameter>") {
            Some(ve) => (&s2[..ve], &s2[ve + "</parameter>".len()..]),
            None => (s2, ""),
        };
        let value = if trim { value.trim() } else { value };
        args.insert(key.to_string(), typed(value));
        s = after;
    }
    args
}

/// DeepSeek 的一组（或一个）调用。名字对不上的那个调用原样留作文字，别的照常换
fn deepseek_block(rest: &str, form: Form, tools: &HashSet<String>) -> Option<(Vec<Piece>, usize)> {
    let group = form == Form::DeepSeekGroup;
    let mut pos = if group { DS_CALLS_BEGIN.len() } else { 0 };
    let (region_end, consumed) = match rest.find(DS_CALLS_END).filter(|_| group) {
        Some(i) => (i, i + DS_CALLS_END.len()),
        None => (rest.len(), rest.len()),
    };
    let mut pieces = Vec::new();
    let mut matched = false;
    while let Some(b) = rest[pos..region_end].find(DS_CALL_BEGIN) {
        let start = pos + b;
        push_text(&mut pieces, &rest[pos..start]);
        let body_at = start + DS_CALL_BEGIN.len();
        let (body, next) = match rest[body_at..region_end].find(DS_CALL_END) {
            Some(e) => (&rest[body_at..body_at + e], body_at + e + DS_CALL_END.len()),
            None => (&rest[body_at..region_end], region_end),
        };
        match deepseek_call(body).filter(|c| tools.contains(&c.name)) {
            Some(c) => {
                matched = true;
                pieces.push(Piece::Call(Box::new(c)));
            }
            None => push_text(&mut pieces, &rest[start..next]),
        }
        pos = next;
        if !group {
            break;
        }
    }
    if !matched {
        return None;
    }
    push_text(&mut pieces, &rest[pos..region_end]);
    Some((pieces, if group { consumed } else { pos }))
}

/// `function<｜tool▁sep｜>名字\n```json\n{…}\n```` 或者 `名字<｜tool▁sep｜>{…}`
fn deepseek_call(body: &str) -> Option<ToolCall> {
    let (head, tail) = body.split_once(DS_SEP)?;
    let (name, args) = match head.trim() {
        "function" => tail.trim_start().split_once('\n').unwrap_or((tail, "")),
        name => (name, tail),
    };
    let name = name.trim();
    if !plausible_name(name) {
        return None;
    }
    let mut args = args.trim();
    if let Some(fenced) = args.strip_prefix("```") {
        // 围栏的第一行是语言名（`json`），收尾的围栏在截断时可能没有
        args = fenced.split_once('\n').map(|(_, a)| a).unwrap_or("");
        args = args.trim().strip_suffix("```").unwrap_or(args).trim();
    }
    let input = if args.is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(args).ok()?
    };
    Some(ToolCall {
        id: new_id("call_"),
        name: name.to_string(),
        input: ToolInput::Json(input),
    })
}

/// 一个值：是 JSON 的数字、布尔、对象、数组、null 就按那个类型，否则是字符串
fn typed(raw: &str) -> Value {
    match serde_json::from_str::<Value>(raw.trim()) {
        Ok(Value::String(_)) | Err(_) => Value::String(raw.to_string()),
        Ok(v) => v,
    }
}

/// 像个工具名：非空、没有空白和标签字符
fn plausible_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(|c: char| c.is_whitespace() || c == '<' || c == '>')
}

/// 整包回答：正文里的调用换成结构化的调用。已经有结构化调用的不碰
pub fn rewrite(r: &mut Response, tools: &HashSet<String>) {
    if tools.is_empty() || r.blocks.iter().any(|b| matches!(b, Block::ToolCall(_))) {
        return;
    }
    let mut synthesized = false;
    let mut blocks = Vec::with_capacity(r.blocks.len());
    for b in r.blocks.drain(..) {
        match b {
            Block::Text(t) => {
                for p in split(&t, tools) {
                    blocks.push(match p {
                        Piece::Text(t) => Block::Text(t),
                        Piece::Call(c) => {
                            synthesized = true;
                            Block::ToolCall(*c)
                        }
                    });
                }
            }
            other => blocks.push(other),
        }
    }
    r.blocks = blocks;
    // 上游以为自己只是在说话（`stop`）：客户端要看到的是「要调工具」
    if synthesized && matches!(r.stop, None | Some(StopReason::EndTurn)) {
        r.stop = Some(StopReason::ToolUse);
    }
}

// ───────────────────────────────────────────────────────── 流

/// 一块文本的流式扫描：开头标记之前的文字照常放行，见到开头标记就攒，攒到收尾标记
/// （或流结束）再解析。可能是标记开头的尾巴（`<tool_c`）先扣住，等下一片
#[derive(Debug)]
pub struct Scanner {
    tools: HashSet<String>,
    pending: String,
    buffering: Option<Form>,
}

impl Scanner {
    pub fn new(tools: HashSet<String>) -> Scanner {
        Scanner {
            tools,
            pending: String::new(),
            buffering: None,
        }
    }

    pub fn feed(&mut self, delta: &str) -> Vec<Piece> {
        self.pending.push_str(delta);
        let mut out = Vec::new();
        loop {
            if let Some(form) = self.buffering {
                let close = form.close();
                let Some(i) = self.pending[form.open().len()..].find(close) else {
                    return out;
                };
                let end = form.open().len() + i + close.len();
                let block = self.pending[..end].to_string();
                self.pending.drain(..end);
                self.buffering = None;
                out.extend(split(&block, &self.tools));
                continue;
            }
            match next_marker(&self.pending) {
                Some((at, form)) => {
                    if at > 0 {
                        out.push(Piece::Text(self.pending[..at].to_string()));
                        self.pending.drain(..at);
                    }
                    self.buffering = Some(form);
                }
                None => {
                    let hold = self
                        .pending
                        .rfind('<')
                        .filter(|&i| {
                            let tail = &self.pending[i..];
                            FORMS.iter().any(|f| f.open().starts_with(tail))
                        })
                        .unwrap_or(self.pending.len());
                    if hold > 0 {
                        out.push(Piece::Text(self.pending[..hold].to_string()));
                        self.pending.drain(..hold);
                    }
                    return out;
                }
            }
        }
    }

    /// 文本块结束：攒着的按截断的写法解析，扣住的尾巴放行
    pub fn finish(&mut self) -> Vec<Piece> {
        self.buffering = None;
        let rest = std::mem::take(&mut self.pending);
        if rest.is_empty() {
            Vec::new()
        } else {
            split(&rest, &self.tools)
        }
    }

    /// 攒着的东西原样当文字交出，不解析
    fn drain_text(&mut self) -> Option<Piece> {
        self.buffering = None;
        let rest = std::mem::take(&mut self.pending);
        (!rest.is_empty()).then_some(Piece::Text(rest))
    }
}

/// 正在扫的那个文本块
#[derive(Debug)]
struct Scanning {
    /// 解析器给的下标
    source: usize,
    /// 写出去的下标：第一段放行的文字到了才开块，整块都是调用的话不开
    out: Option<usize>,
    scanner: Scanner,
}

/// 解析器和写出器之间的一道：Chat 上游的流里，文本块里的调用换成调用块。
///
/// **块重新编号**：换出来的调用块要有自己的下标，所以经过这里的每个块都拿一个新下标。
/// 见到结构化的调用块之后不再认文字里的（上游会写 `tool_calls` 就不用猜了）；在那之前
/// 已经换出来的不收回 —— 流已经发出去了。结束原因在文本块扫完之前扣住，换出过调用时
/// `stop` 改报 `tool_use`
#[derive(Debug)]
pub struct Stage {
    tools: HashSet<String>,
    map: HashMap<usize, usize>,
    next: usize,
    structured: bool,
    text: Option<Scanning>,
    held_stop: Option<StopReason>,
    synthesized: bool,
}

impl Stage {
    pub fn new(tools: HashSet<String>) -> Stage {
        Stage {
            tools,
            map: HashMap::new(),
            next: 0,
            structured: false,
            text: None,
            held_stop: None,
            synthesized: false,
        }
    }

    fn fresh(&mut self) -> usize {
        let i = self.next;
        self.next += 1;
        i
    }

    fn mapped(&self, index: usize) -> usize {
        self.map.get(&index).copied().unwrap_or(index)
    }

    pub fn apply(&mut self, e: Event) -> Vec<Event> {
        let mut out = Vec::new();
        match e {
            Event::BlockStart {
                index,
                kind: BlockKind::Text,
            } if !self.structured => {
                self.text = Some(Scanning {
                    source: index,
                    out: None,
                    scanner: Scanner::new(self.tools.clone()),
                });
            }
            Event::BlockStart { index, kind } => {
                if matches!(kind, BlockKind::ToolCall { .. }) {
                    self.structured = true;
                    // 不该发生（Chat 的解析器开新块前先结束上一块），发生了就把攒着的
                    // 原样当文字放行：有结构化的调用，文字里的不算
                    if let Some(mut s) = self.text.take() {
                        let piece = s.scanner.drain_text();
                        self.emit(&mut s, piece.into_iter(), &mut out);
                        self.close_text(s, &mut out);
                    }
                }
                let i = self.fresh();
                self.map.insert(index, i);
                out.push(Event::BlockStart { index: i, kind });
            }
            Event::Delta {
                index,
                delta: Delta::Text(t),
            } if self.text.as_ref().is_some_and(|s| s.source == index) => {
                let mut s = self.text.take().unwrap();
                let pieces = s.scanner.feed(&t);
                self.emit(&mut s, pieces.into_iter(), &mut out);
                self.text = Some(s);
            }
            Event::Delta { index, delta } => out.push(Event::Delta {
                index: self.mapped(index),
                delta,
            }),
            Event::BlockStop { index } if self.text.as_ref().is_some_and(|s| s.source == index) => {
                let mut s = self.text.take().unwrap();
                let pieces = s.scanner.finish();
                self.emit(&mut s, pieces.into_iter(), &mut out);
                self.close_text(s, &mut out);
                if let Some(stop) = self.held_stop.take() {
                    out.push(self.stop(stop));
                }
            }
            Event::BlockStop { index } => out.push(Event::BlockStop {
                index: self.mapped(index),
            }),
            Event::Stop(s) => {
                if self.text.is_some() {
                    self.held_stop = Some(s);
                } else {
                    out.push(self.stop(s));
                }
            }
            other => out.push(other),
        }
        out
    }

    fn stop(&self, s: StopReason) -> Event {
        Event::Stop(if self.synthesized && s == StopReason::EndTurn {
            StopReason::ToolUse
        } else {
            s
        })
    }

    fn emit(
        &mut self,
        s: &mut Scanning,
        pieces: impl Iterator<Item = Piece>,
        out: &mut Vec<Event>,
    ) {
        for p in pieces {
            match p {
                Piece::Text(t) => {
                    let index = match s.out {
                        Some(i) => i,
                        None => {
                            let i = self.fresh();
                            s.out = Some(i);
                            out.push(Event::BlockStart {
                                index: i,
                                kind: BlockKind::Text,
                            });
                            i
                        }
                    };
                    out.push(Event::Delta {
                        index,
                        delta: Delta::Text(t),
                    });
                }
                Piece::Call(c) => {
                    let c = *c;
                    if let Some(i) = s.out.take() {
                        out.push(Event::BlockStop { index: i });
                    }
                    self.synthesized = true;
                    let index = self.fresh();
                    out.push(Event::BlockStart {
                        index,
                        kind: BlockKind::ToolCall {
                            id: c.id,
                            name: c.name,
                        },
                    });
                    out.push(Event::Delta {
                        index,
                        delta: Delta::ToolInput(c.input.to_json_text()),
                    });
                    out.push(Event::BlockStop { index });
                }
            }
        }
    }

    fn close_text(&mut self, s: Scanning, out: &mut Vec<Event>) {
        if let Some(i) = s.out {
            out.push(Event::BlockStop { index: i });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tools(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn call(p: &Piece) -> &ToolCall {
        match p {
            Piece::Call(c) => c,
            Piece::Text(t) => panic!("不是调用：{t:?}"),
        }
    }

    #[test]
    fn a_tagged_json_call_becomes_a_call_and_the_prose_around_it_stays() {
        let out = split(
            "先看一下。\n<tool_call>{\"name\": \"Read\", \"arguments\": {\"path\": \"a.rs\"}}</tool_call>\n然后再说。",
            &tools(&["Read"]),
        );
        assert_eq!(out.len(), 3, "{out:?}");
        assert_eq!(out[0], Piece::Text("先看一下。\n".into()));
        let c = call(&out[1]);
        assert_eq!(c.name, "Read");
        assert_eq!(c.input, ToolInput::Json(json!({"path": "a.rs"})));
        assert!(c.id.starts_with("call_"), "{}", c.id);
        assert_eq!(out[2], Piece::Text("\n然后再说。".into()));
    }

    #[test]
    fn parameters_and_string_arguments_are_accepted_too() {
        let out = split(
            "<tool_call>{\"name\": \"ls\", \"parameters\": {\"p\": \".\"}}</tool_call><tool_call>{\"name\": \"ls\", \"arguments\": \"{\\\"p\\\": \\\"/\\\"}\"}</tool_call>",
            &tools(&["ls"]),
        );
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(call(&out[0]).input, ToolInput::Json(json!({"p": "."})));
        assert_eq!(call(&out[1]).input, ToolInput::Json(json!({"p": "/"})));
    }

    #[test]
    fn a_name_the_request_did_not_define_stays_text() {
        let text = "<tool_call>{\"name\": \"rm\", \"arguments\": {}}</tool_call>";
        assert_eq!(
            split(text, &tools(&["Read"])),
            vec![Piece::Text(text.into())]
        );
        // 后面那个是真的：前面认不出的不挡路
        let out = split(
            &format!("{text}<tool_call>{{\"name\": \"Read\", \"arguments\": {{}}}}</tool_call>"),
            &tools(&["Read"]),
        );
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0], Piece::Text(text.into()));
        assert_eq!(call(&out[1]).name, "Read");
    }

    #[test]
    fn a_bare_json_object_in_prose_is_never_a_call() {
        let text = "像这样写：{\"name\": \"Read\", \"arguments\": {\"path\": \"a\"}}";
        assert_eq!(
            split(text, &tools(&["Read"])),
            vec![Piece::Text(text.into())]
        );
        assert_eq!(split("", &tools(&["Read"])), vec![]);
    }

    #[test]
    fn the_deepseek_form_with_two_calls_and_no_closing_marker() {
        let text = "我来查。<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>function<｜tool▁sep｜>get_weather\n```json\n{\"city\": \"北京\"}\n```<｜tool▁call▁end｜><｜tool▁call▁begin｜>function<｜tool▁sep｜>get_time\n```json\n{}\n```<｜tool▁call▁end｜><｜tool▁calls▁end｜>完。";
        let out = split(text, &tools(&["get_weather", "get_time"]));
        assert_eq!(out.len(), 4, "{out:?}");
        assert_eq!(out[0], Piece::Text("我来查。".into()));
        assert_eq!(call(&out[1]).name, "get_weather");
        assert_eq!(
            call(&out[1]).input,
            ToolInput::Json(json!({"city": "北京"}))
        );
        assert_eq!(call(&out[2]).input, ToolInput::Json(json!({})));
        assert_eq!(out[3], Piece::Text("完。".into()));

        // 到上限截断：收尾的围栏和两个结束标记都没有
        let cut = "<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>function<｜tool▁sep｜>get_weather\n```json\n{\"city\": \"北京\"}";
        let out = split(cut, &tools(&["get_weather"]));
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(
            call(&out[0]).input,
            ToolInput::Json(json!({"city": "北京"}))
        );

        // 新写法：不写 function、不带围栏、没有外层标记
        let v31 =
            "<｜tool▁call▁begin｜>get_time<｜tool▁sep｜>{\"zone\": \"UTC\"}<｜tool▁call▁end｜>";
        let out = split(v31, &tools(&["get_time"]));
        assert_eq!(call(&out[0]).input, ToolInput::Json(json!({"zone": "UTC"})));

        // 一组里名字对不上的那个留作文字
        let out = split(text, &tools(&["get_time"]));
        assert_eq!(out.len(), 3, "{out:?}");
        assert!(matches!(&out[0], Piece::Text(t) if t.contains("get_weather")));
        assert_eq!(call(&out[1]).name, "get_time");
        // 全都对不上：整段是文字
        assert_eq!(split(text, &tools(&["x"])), vec![Piece::Text(text.into())]);
    }

    #[test]
    fn the_glm_form_types_its_values() {
        let text = "<tool_call>search\n<arg_key>q</arg_key>\n<arg_value>rust 2024</arg_value>\n<arg_key>limit</arg_key>\n<arg_value>5</arg_value>\n<arg_key>deep</arg_key>\n<arg_value>true</arg_value>\n<arg_key>opts</arg_key>\n<arg_value>{\"a\": [1]}</arg_value>\n</tool_call>";
        let out = split(text, &tools(&["search"]));
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(
            call(&out[0]).input,
            ToolInput::Json(
                json!({"q": "rust 2024", "limit": 5, "deep": true, "opts": {"a": [1]}})
            )
        );
        // 截断：最后一个值没有收尾
        let out = split(
            "<tool_call>search\n<arg_key>q</arg_key>\n<arg_value>rus",
            &tools(&["search"]),
        );
        assert_eq!(call(&out[0]).input, ToolInput::Json(json!({"q": "rus"})));
        // 名字对不上
        assert_eq!(
            split(text, &tools(&["Read"])),
            vec![Piece::Text(text.into())]
        );
    }

    #[test]
    fn the_function_form_with_and_without_its_closing_tags() {
        let text = "好的。<function=Write><parameter name=\"path\">a.txt</parameter><parameter name=\"n\">3</parameter></function>";
        let out = split(text, &tools(&["Write"]));
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(
            call(&out[1]).input,
            ToolInput::Json(json!({"path": "a.txt", "n": 3}))
        );
        let cut = "<function=Write><parameter name=\"path\">a.txt</parameter><parameter name=\"body\">hello";
        let out = split(cut, &tools(&["Write"]));
        assert_eq!(
            call(&out[0]).input,
            ToolInput::Json(json!({"path": "a.txt", "body": "hello"}))
        );
        // 另一种参数写法，和 JSON 正文
        let out = split(
            "<function=Write>\n<parameter=path>\na.txt\n</parameter>\n</function><function=Write>{\"path\": \"b\"}</function>",
            &tools(&["Write"]),
        );
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(
            call(&out[0]).input,
            ToolInput::Json(json!({"path": "a.txt"}))
        );
        assert_eq!(call(&out[1]).input, ToolInput::Json(json!({"path": "b"})));
        assert_eq!(
            split(text, &tools(&["Read"])),
            vec![Piece::Text(text.into())]
        );
    }

    #[test]
    fn a_whole_response_is_rewritten_only_when_it_has_no_structured_calls() {
        let mut r = Response {
            blocks: vec![Block::Text(
                "<tool_call>{\"name\": \"Read\", \"arguments\": {}}</tool_call>".into(),
            )],
            stop: Some(StopReason::EndTurn),
            ..Default::default()
        };
        rewrite(&mut r, &tools(&["Read"]));
        assert!(matches!(&r.blocks[0], Block::ToolCall(c) if c.name == "Read"));
        assert_eq!(r.stop, Some(StopReason::ToolUse));

        let text = "<tool_call>{\"name\": \"Read\", \"arguments\": {}}</tool_call>";
        let mut r = Response {
            blocks: vec![
                Block::Text(text.into()),
                Block::ToolCall(ToolCall {
                    id: "c".into(),
                    name: "ls".into(),
                    input: ToolInput::Json(json!({})),
                }),
            ],
            ..Default::default()
        };
        rewrite(&mut r, &tools(&["Read", "ls"]));
        assert_eq!(r.blocks[0], Block::Text(text.into()));
        let mut r = Response {
            blocks: vec![Block::Text(text.into())],
            ..Default::default()
        };
        rewrite(&mut r, &HashSet::new());
        assert_eq!(r.blocks[0], Block::Text(text.into()));
    }

    #[test]
    fn the_scanner_streams_prose_and_holds_a_marker_until_it_closes() {
        let mut s = Scanner::new(tools(&["Read"]));
        assert_eq!(s.feed("我先看"), vec![Piece::Text("我先看".into())]);
        // 尾巴可能是标记的开头：扣住
        assert_eq!(s.feed("一下。<tool_"), vec![Piece::Text("一下。".into())]);
        assert_eq!(s.feed("call>{\"name\": \"Re"), vec![]);
        let out = s.feed("ad\", \"arguments\": {}}</tool_call>再");
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(call(&out[0]).name, "Read");
        assert_eq!(out[1], Piece::Text("再".into()));
        // `<` 不是标记的开头：照常放行
        assert_eq!(s.feed("说 a<b"), vec![Piece::Text("说 a<b".into())]);
        assert_eq!(s.finish(), vec![]);

        // 没有收尾标记：块结束时按截断解析
        let mut s = Scanner::new(tools(&["Read"]));
        assert_eq!(s.feed("<function=Read><parameter name=\"path\">a"), vec![]);
        let out = s.finish();
        assert_eq!(call(&out[0]).input, ToolInput::Json(json!({"path": "a"})));

        // 名字对不上：攒着的原样放行
        let mut s = Scanner::new(tools(&["Read"]));
        assert_eq!(s.feed("<tool_call>{\"name\": \"rm\"}"), vec![]);
        assert_eq!(
            s.finish(),
            vec![Piece::Text("<tool_call>{\"name\": \"rm\"}".into())]
        );
    }

    #[test]
    fn the_stage_turns_a_text_block_into_text_call_text_and_reports_tool_use() {
        let mut st = Stage::new(tools(&["Read"]));
        let mut ev = Vec::new();
        for e in [
            Event::Start {
                id: None,
                model: None,
            },
            Event::BlockStart {
                index: 0,
                kind: BlockKind::Thinking,
            },
            Event::BlockStop { index: 0 },
            Event::BlockStart {
                index: 1,
                kind: BlockKind::Text,
            },
            Event::Delta {
                index: 1,
                delta: Delta::Text("先看。".into()),
            },
            Event::Delta {
                index: 1,
                delta: Delta::Text(
                    "<tool_call>{\"name\": \"Read\", \"arguments\": {\"p\": 1}}</tool_call>".into(),
                ),
            },
            Event::Delta {
                index: 1,
                delta: Delta::Text("再说。".into()),
            },
            Event::Stop(StopReason::EndTurn),
            Event::BlockStop { index: 1 },
        ] {
            ev.extend(st.apply(e));
        }
        let kinds: Vec<&BlockKind> = ev
            .iter()
            .filter_map(|e| match e {
                Event::BlockStart { kind, .. } => Some(kind),
                _ => None,
            })
            .collect();
        assert_eq!(kinds.len(), 4, "{ev:#?}");
        assert_eq!(kinds[0], &BlockKind::Thinking);
        assert_eq!(kinds[1], &BlockKind::Text);
        assert!(matches!(kinds[2], BlockKind::ToolCall { name, .. } if name == "Read"));
        assert_eq!(kinds[3], &BlockKind::Text);
        assert!(ev.contains(&Event::Delta {
            index: 2,
            delta: Delta::ToolInput("{\"p\":1}".into())
        }));
        // 结束原因在最后，而且改报要调工具
        assert_eq!(ev.last(), Some(&Event::Stop(StopReason::ToolUse)));
        let stops = ev
            .iter()
            .filter(|e| matches!(e, Event::BlockStop { .. }))
            .count();
        assert_eq!(stops, 4, "{ev:#?}");
    }

    #[test]
    fn a_text_block_that_is_only_a_call_opens_no_text_block() {
        let mut st = Stage::new(tools(&["Read"]));
        let mut ev = Vec::new();
        for e in [
            Event::BlockStart {
                index: 0,
                kind: BlockKind::Text,
            },
            Event::Delta {
                index: 0,
                delta: Delta::Text("<tool_call>{\"name\": \"Read\"}".into()),
            },
            Event::BlockStop { index: 0 },
            Event::Stop(StopReason::MaxTokens),
        ] {
            ev.extend(st.apply(e));
        }
        assert_eq!(ev.len(), 4, "{ev:#?}");
        assert!(matches!(
            &ev[0],
            Event::BlockStart {
                kind: BlockKind::ToolCall { .. },
                ..
            }
        ));
        assert_eq!(ev[3], Event::Stop(StopReason::MaxTokens));
    }

    #[test]
    fn after_a_structured_call_text_is_left_alone() {
        let mut st = Stage::new(tools(&["Read"]));
        let mut ev = Vec::new();
        let text = "<tool_call>{\"name\": \"Read\"}</tool_call>";
        for e in [
            Event::BlockStart {
                index: 0,
                kind: BlockKind::ToolCall {
                    id: "c".into(),
                    name: "ls".into(),
                },
            },
            Event::BlockStop { index: 0 },
            Event::BlockStart {
                index: 1,
                kind: BlockKind::Text,
            },
            Event::Delta {
                index: 1,
                delta: Delta::Text(text.into()),
            },
            Event::BlockStop { index: 1 },
        ] {
            ev.extend(st.apply(e));
        }
        assert_eq!(
            ev[3],
            Event::Delta {
                index: 1,
                delta: Delta::Text(text.into())
            }
        );
    }
}
