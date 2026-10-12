//! 一份 JSON 原文从头读到这儿，放回去的原值要转义几次。
//!
//! 占位符里没有要转义的字，所以不管它在第几层字符串里，原文里都是原样写着的 —— 光看它
//! 本身说不出要转义几次。一个字符串里放的是一段话（正文）：原值转义一次。放的是一段 JSON
//! （工具调用的参数，Chat、Responses 的 `arguments`，Converse 的 `toolUse.input`）、占位符
//! 又在那段 JSON 的一个字符串里：要转义两次 —— 解开外面这层是参数那段 JSON，再解开一层才是
//! 值。只转义一次的话，客户端解出来的参数里夹着一个真的换行，解析不了。
//!
//! **一层套一层地读**：参数那段 JSON 里又有一个放 JSON 的字段（工具自己的参数叫
//! `arguments`、里面是一段 JSON）时，再往里一层，转义三次，多少层都一样。每一层是同一个
//! 读法（[`Reader`]），读的是外面那层字符串解开之后的字。
//!
//! **一边往下读一边记**，不解析成一棵树再写回去：整份的回答、按块到的字节流、WebSocket 的
//! 一帧都用它，没有占位符的地方一个字节都不动，流也不必攒齐了才发。
//!
//! 哪些字符串里放的是 JSON，和按帧还原（[`crate::redact::sse`]）认的是同一批键，而且得真
//! 长得像 JSON（第一个不是空白的字是 `{` 或 `[`）：工具自己的参数里也可能有个叫 `arguments`
//! 的字段，它放的是一句话。

/// 一个键对还原来说是哪一种
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Key {
    #[default]
    Other,
    /// `arguments`、`partial_json`：工具调用的参数
    Args,
    /// `input`：在 `toolUse` 下面才是参数（Converse）
    Input,
    ToolUse,
}

fn classify(raw: &[u8]) -> Key {
    match raw {
        b"arguments" | b"partial_json" => Key::Args,
        b"input" => Key::Input,
        b"toolUse" => Key::ToolUse,
        _ => Key::Other,
    }
}

/// 分类只看键的头几个字节：认的那几个键都比它短
const HEAD: usize = 16;

/// 一层字符串里放的东西读到了哪儿
#[derive(Debug, Clone, Default)]
enum Inner {
    /// 不是放参数的字符串
    #[default]
    Off,
    /// 放参数的键下面，还没读到不是空白的字
    Undecided,
    /// 不像 JSON：一句话
    Plain,
    /// 一段 JSON：往里再读一层
    Json(Box<Reader>),
}

/// 读到哪儿了（见模块说明）。
#[derive(Debug, Clone, Default)]
pub(crate) struct Nest {
    /// 读到过不是空白的字没有；读到过的话，开头是不是 JSON（`{`、`[`、`"`）。不是 JSON 的
    /// 文字不往下看，原值一律转义一次
    json: Option<bool>,
    top: Reader,
}

impl Nest {
    /// 此刻放进去的原值要转义几次
    pub(crate) fn level(&self) -> usize {
        match self.json {
            Some(true) => self.top.depth().max(1),
            _ => 1,
        }
    }

    /// 接着往下读 `s`
    pub(crate) fn advance(&mut self, s: &[u8]) {
        let mut i = 0;
        while i < s.len() {
            match self.json {
                None if s[i].is_ascii_whitespace() => {
                    i += 1;
                    continue;
                }
                None => self.json = Some(matches!(s[i], b'{' | b'[' | b'"')),
                Some(false) => return,
                Some(true) => {}
            }
            // 一个字符串里不会改变什么的字整段跳过：只等引号和反斜杠
            if self.top.plain_run() {
                match memchr::memchr2(b'"', b'\\', &s[i..]) {
                    Some(n) => i += n,
                    None => return,
                }
            }
            self.top.feed(s[i]);
            i += 1;
        }
    }
}

/// 一层 JSON 读到了哪儿：最外面一层读原文的字节，里面每一层读外面那层字符串解开之后的字。
#[derive(Debug, Clone, Default)]
struct Reader {
    in_str: bool,
    esc: bool,
    /// `\uXXXX` 还差几位、读到的值
    hex: u8,
    code: u32,
    /// 这个字符串可能是一个键（前面没有冒号在等值）
    maybe_key: bool,
    /// 这个字符串的头几个字节：读完是一个键的话按它分类
    head: Vec<u8>,
    /// 刚读完的那个字符串
    last: Key,
    /// 读到冒号、在等值的那个键
    pending: Option<Key>,
    /// 每一层容器是哪个键的值
    stack: Vec<Key>,
    /// 此刻这个字符串里放的东西
    inner: Inner,
}

impl Reader {
    /// 此刻在第几层字符串里：这一层不在字符串里是 0
    fn depth(&self) -> usize {
        if !self.in_str {
            return 0;
        }
        1 + match &self.inner {
            Inner::Json(r) => r.depth(),
            _ => 0,
        }
    }

    /// 接下来的字只要不是引号和反斜杠，就什么都不改变
    fn plain_run(&self) -> bool {
        self.in_str
            && !self.esc
            && self.hex == 0
            && matches!(self.inner, Inner::Off | Inner::Plain)
            && (!self.maybe_key || self.head.len() >= HEAD)
    }

    fn feed(&mut self, b: u8) {
        if self.in_str {
            self.string_byte(b);
        } else {
            self.structure(b);
        }
    }

    fn structure(&mut self, b: u8) {
        match b {
            b'"' => {
                self.in_str = true;
                let key = self.pending.take();
                self.maybe_key = key.is_none();
                self.head.clear();
                let parent = self.stack.last().copied().unwrap_or_default();
                self.inner = match key {
                    Some(Key::Args) => Inner::Undecided,
                    Some(Key::Input) if parent == Key::ToolUse => Inner::Undecided,
                    _ => Inner::Off,
                };
            }
            b':' => self.pending = Some(self.last),
            b'{' | b'[' => {
                let key = self.pending.take().unwrap_or_default();
                self.stack.push(key);
            }
            b'}' | b']' => {
                self.stack.pop();
                self.pending = None;
            }
            b',' => self.pending = None,
            _ => {}
        }
    }

    fn string_byte(&mut self, b: u8) {
        if self.hex > 0 {
            self.code = self.code * 16 + (b as char).to_digit(16).unwrap_or(0);
            self.hex -= 1;
            if self.hex == 0 {
                // 只有引号、反斜杠和括号对里面那段 JSON 有意义，别的都当一个普通的字
                let c = u8::try_from(self.code).unwrap_or(b'x');
                self.decoded(c);
            }
            return;
        }
        if self.esc {
            self.esc = false;
            match b {
                b'u' => {
                    self.hex = 4;
                    self.code = 0;
                }
                b'"' | b'\\' | b'/' => self.decoded(b),
                // `\n`、`\t` 这些：里面那段 JSON 的空白
                _ => self.decoded(b' '),
            }
            return;
        }
        match b {
            b'\\' => self.esc = true,
            b'"' => {
                self.in_str = false;
                self.last = if self.maybe_key {
                    classify(&self.head)
                } else {
                    Key::Other
                };
                self.head.clear();
                self.inner = Inner::Off;
            }
            _ => {
                if self.maybe_key && self.head.len() < HEAD {
                    self.head.push(b);
                }
                self.decoded(b);
            }
        }
    }

    /// 这一层字符串解开之后的一个字节，交给里面那段 JSON
    fn decoded(&mut self, c: u8) {
        match &mut self.inner {
            Inner::Off | Inner::Plain => {}
            Inner::Undecided => {
                if !c.is_ascii_whitespace() {
                    self.inner = if c == b'{' || c == b'[' {
                        let mut r = Box::<Reader>::default();
                        r.feed(c);
                        Inner::Json(r)
                    } else {
                        Inner::Plain
                    };
                }
            }
            Inner::Json(r) => r.feed(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `text` 里每一处 `@` 的层数
    fn levels(text: &str) -> Vec<usize> {
        let mut n = Nest::default();
        let mut out = Vec::new();
        let mut from = 0;
        for (at, _) in text.match_indices('@') {
            n.advance(&text.as_bytes()[from..at]);
            out.push(n.level());
            from = at;
        }
        out
    }

    #[test]
    fn a_string_holding_json_is_one_level_deeper() {
        let chat = r#"{"choices":[{"message":{"content":"@","tool_calls":[{"function":{"name":"w","arguments":"{\"v\":\"@\",\"n\":1}"}}]}}]}"#;
        assert_eq!(levels(chat), [1, 2]);
        // 参数那段 JSON 里、字符串外面的（坏掉的参数）照一层
        assert_eq!(levels(r#"{"arguments":"{\"v\":@}"}"#), [1]);
        // 字符串里转义过的引号不算收尾
        assert_eq!(
            levels(r#"{"arguments":"{\"v\":\"a\\\"b @\",\"w\":\"@\"}"}"#),
            [2, 2]
        );
        // `\u0022` 就是引号
        assert_eq!(
            levels(r#"{"arguments":"{\u0022v\u0022:\u0022@\u0022}"}"#),
            [2]
        );
    }

    #[test]
    fn json_inside_json_inside_json_is_one_level_deeper_each_time() {
        use serde_json::json;
        // 工具自己的参数里有个 `arguments`，放的又是一段 JSON
        let inner = json!({ "content": "@" }).to_string();
        let args = json!({ "tool": "w", "arguments": inner, "x": "@" }).to_string();
        let chat = json!({ "arguments": args }).to_string();
        assert_eq!(levels(&chat), [3, 2]);
        // 再往里一层
        let deeper = json!({ "arguments": json!({ "arguments": chat }).to_string() }).to_string();
        assert_eq!(levels(&deeper), [5, 4]);
        // 读完一层回到外面
        let back = json!({ "arguments": json!({ "arguments": "{}" }).to_string(), "t": "@" });
        assert_eq!(levels(&back.to_string()), [1]);
    }

    #[test]
    fn only_protocol_keys_that_hold_json_count() {
        // Converse：`toolUse` 下面的 `input`
        assert_eq!(
            levels(r#"{"delta":{"toolUse":{"input":"{\"v\":\"@\"}"}},"input":"{\"v\":\"@\"}"}"#),
            [2, 1]
        );
        // 工具自己的参数里叫 `arguments` 的字段，放的是一句话
        assert_eq!(levels(r#"{"input":{"arguments":"照 \"@\" 写"}}"#), [1]);
        // 放的是对象（Anthropic 的 `input`）：一层
        assert_eq!(
            levels(r#"{"input":{"v":"@"},"arguments":{"v":"@"}}"#),
            [1, 1]
        );
        // 键是另一个键的值时不算键
        assert_eq!(levels(r#"{"name":"arguments","x":"{\"v\":\"@\"}"}"#), [1]);
    }

    #[test]
    fn text_that_is_not_json_is_one_level_everywhere() {
        assert_eq!(
            levels("event: x\ndata: {\"arguments\":\"{\\\"v\\\":\\\"@\\\"}\"}"),
            [1]
        );
        assert_eq!(levels("你说的是 @"), [1]);
    }

    #[test]
    fn reading_in_pieces_is_reading_it_whole() {
        let text = r#"  {"a":[1,{"arguments":"{\"k\":\"x\\\"y\",\"v\":\"@\"}"}],"toolUse":{"input":"[\"@\"]"},"b":"\u00e9@"}"#;
        let whole = levels(text);
        assert_eq!(whole, [2, 2, 1]);
        for size in 1..12 {
            let mut n = Nest::default();
            let mut got = Vec::new();
            let mut from = 0;
            for (at, _) in text.match_indices('@') {
                for piece in text.as_bytes()[from..at].chunks(size) {
                    n.advance(piece);
                }
                got.push(n.level());
                from = at;
            }
            assert_eq!(got, whole, "{size}");
        }
    }
}
