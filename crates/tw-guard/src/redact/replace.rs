//! 把敏感值换成占位符，并记住怎么换回来。
//!
//! **可逆替换，不是删除。**删掉的话模型看到半截连接串反而会瞎猜 ——
//! 而「瞎猜」在一个正帮你调试 `.env` 的助手身上，比看不见更糟。

use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use crate::redact::nest::Nest;
use crate::redact::rules::{Hit, RuleSet};

/// 占位符长什么样：`{open}{label}_{n}{close}`。
///
/// **两边长得不一样，引擎是同一个。**桌面版是 `<<TW_SECRET_1>>`：开发者的
/// 请求里满是 `{{ }}` 模板，用尖括号撞车的机会少得多。企业版是
/// `{{EMAIL_1}}`：标签告诉模型「这里原来是个邮箱」，它才答得像样。
///
/// 开头那段必须是 ASCII：流式还原按字节找它，找到的位置要能直接切。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scheme {
    pub open: &'static str,
    pub close: &'static str,
    /// 规则没有自己的标签时用它
    pub label: &'static str,
}

impl Scheme {
    /// 桌面版的：`<<TW_SECRET_1>>`
    pub const SECRET: Scheme = Scheme {
        open: "<<",
        close: ">>",
        label: "TW_SECRET",
    };

    pub fn placeholder(&self, label: &str, n: usize) -> String {
        format!("{}{label}_{n}{}", self.open, self.close)
    }

    /// `text` 里写着的每一个这种写法的占位符：在哪儿（字节区间）、标签、号码。
    ///
    /// **夹在别的字里的也算**（`<<<TW_SECRET_1>>`）：还原是按子串换的。号码大到装不下的
    /// 不算 —— 那不可能是发出去的。
    pub fn find_in<'a>(&self, text: &'a str) -> Vec<(Range<usize>, &'a str, usize)> {
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(i) = text[from..].find(self.open) {
            let at = from + i;
            let start = at + self.open.len();
            // 下一处从这一处的下一个字节找起：`<<<TW_SECRET_1>>` 里的占位符从第二个 `<` 起。
            // 开头那段是 ASCII，加一还落在字的边界上
            from = at + 1;
            let rest = &text[start..];
            let len = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            if !rest[len..].starts_with(self.close) {
                continue;
            }
            let Some((label, n)) = rest[..len].rsplit_once('_') else {
                continue;
            };
            if label.is_empty() || n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let Ok(n) = n.parse::<usize>() else { continue };
            out.push((at..start + len + self.close.len(), label, n));
        }
        out
    }
}

/// 一段文字是哪种写法：值从哪儿找到的、还原时要放进哪儿去。
///
/// **同一个值在两种写法里长得不一样。**私钥块里的换行在请求体的 JSON 原文里是 `\n` 两个
/// 字符，解开之后是一个换行符；连接串的口令里可以有 `\"`。原文里的样子放进解码过的正文，
/// 客户端看到的是一个反斜杠和一个 n；解开的样子不转义就放回 JSON 原文，整份 JSON 就坏了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// JSON 原文：请求体、WebSocket 的一帧、工具参数那一段 JSON。字符串里的东西带着转义
    Json,
    /// 解码过的正文：JSON 字符串解开之后的那段话。反斜杠就是反斜杠
    Text,
}

/// `s` 写进一个 JSON 字符串里的样子（不带两头的引号）。**和 serde_json 的转义一致**；
/// 没有要转义的字时原样借出去，不抄。
pub fn json_escaped(s: &str) -> Cow<'_, str> {
    if !s.bytes().any(|b| b == b'"' || b == b'\\' || b < 0x20) {
        return Cow::Borrowed(s);
    }
    let quoted = serde_json::to_string(s).unwrap_or_default();
    Cow::Owned(quoted[1..quoted.len().saturating_sub(1)].to_string())
}

/// 一次脱敏留下的账本。
///
/// **同一个值在一次请求里只占一个编号。**一把 key 出现三次却换成三个
/// 不同的占位符，会让模型以为那是三个不同的东西 —— 而它可能正在帮你
/// 对比「这两处的 key 是不是同一把」。
///
/// **编号按标签各数各的**：`{{EMAIL_1}}`、`{{PHONE_1}}`，而不是
/// `{{EMAIL_1}}`、`{{PHONE_2}}` —— 后者让模型以为漏了一个。
///
/// **原值记的是解开转义之后的样子**（[`Form::Text`]），不管是在哪种写法里找到的：还原时
/// 按要放进去的地方再写一遍（[`restore`]、[`restore_text`]），一个值不会被解两次、也不会
/// 被转义两次。请求体里写着 `\n` 的私钥块，和插件那一层在正文里看到的同一个私钥块，因此
/// 是同一个号。
///
/// 换了哪些、各几处不记在这里：那是 [`crate::redact::rules::findings`] 从命中里
/// 算出来的，观察档（不换）和拦截档（换）报的是同一份。
#[derive(Debug, Clone)]
pub struct Ledger {
    scheme: Scheme,
    /// 占位符 → 原值（解开转义之后的）
    back: HashMap<String, String>,
    /// 原值（解开转义之后的）→ 占位符，用来复用编号
    seen: HashMap<String, String>,
    /// 每个标签发到几号了
    issued: HashMap<String, usize>,
    /// 发出去的占位符有哪几种长度，从短到长。还原时在每个开头处按它们各查一次表（见
    /// [`restore`]）；标签就那几个、号的位数也就几种，所以这里只有寥寥几个数
    lens: Vec<usize>,
    /// 在一段文字里一次找出所有原值用的（[`hide_values`]），头一次用时建，记了新账就作废
    finder: std::sync::OnceLock<Option<Arc<ValueFinder>>>,
}

impl Ledger {
    pub fn new(scheme: Scheme) -> Self {
        Self {
            scheme,
            back: HashMap::new(),
            seen: HashMap::new(),
            issued: HashMap::new(),
            lens: Vec::new(),
            finder: std::sync::OnceLock::new(),
        }
    }
    pub fn scheme(&self) -> Scheme {
        self.scheme
    }
    pub fn is_empty(&self) -> bool {
        self.back.is_empty()
    }
    pub fn len(&self) -> usize {
        self.back.len()
    }
    /// 占位符 → 原值，**解开转义之后的样子**。流式还原要拿它。
    pub fn table(&self) -> &HashMap<String, String> {
        &self.back
    }
    /// 发出去的占位符有哪几种长度，从短到长
    pub(crate) fn lens(&self) -> &[usize] {
        &self.lens
    }
    /// 原值 → 占位符。在一处找到的值要换到别处去时用它（企业版在解码后的
    /// 请求上找，再换进原样转发的那一份）。
    ///
    /// 原值是**解开转义之后的样子**：要在 JSON 原文里找，先按 [`json_escaped`] 写一遍。
    pub fn replacements(&self) -> impl Iterator<Item = (&str, &str)> {
        self.seen.iter().map(|(o, p)| (o.as_str(), p.as_str()))
    }
    /// 换掉 `value` 的那个占位符。没换过的是 `None`。`value` 是解开转义之后的样子（JSON
    /// 原文里截下来的一段先 [`crate::locate::unescape`]）
    pub fn placeholder_of(&self, value: &str) -> Option<&str> {
        self.seen.get(value).map(String::as_str)
    }

    /// 让开 `text` 里已经写着的占位符：新发的号接在它们后面数。
    ///
    /// 一段文字里本来就可能写着占位符：存下来的请求（拦截档下存的是换过的那一份）拿去
    /// 重放、导出成用例，用户把请求详情里看到的东西贴回对话。照常从 1 数的话，新找到的
    /// 值会拿到一个已经写在那儿的号 —— 两样东西共用一个占位符，回显里的那一处被还原成
    /// 新值，而它原本指的是另一样。
    ///
    /// **只让号，不记账**：写在那儿的占位符不知道原来是什么，还原时原样留着。
    pub fn avoiding(mut self, text: &str) -> Self {
        for (_, label, n) in self.scheme.find_in(text) {
            let taken = self.issued.entry(label.to_string()).or_insert(0);
            *taken = (*taken).max(n);
        }
        self
    }

    /// 这个值的占位符，头一次见就发一个新号。`original` 是按 `form` 写的：JSON 原文里截下来的
    /// 先解开转义再记账（见 [`Ledger`]）；正文里的原样记 —— 再解一次，正文里本来就有的反斜杠
    /// 就没了。
    fn issue(&mut self, original: &str, label: &str, form: Form) -> String {
        let original = match form {
            Form::Json if original.contains('\\') => Cow::Owned(crate::locate::unescape(original)),
            _ => Cow::Borrowed(original),
        };
        let original = original.as_ref();
        if let Some(p) = self.seen.get(original) {
            return p.clone();
        }
        let n = self.issued.entry(label.to_string()).or_insert(0);
        *n += 1;
        let p = self.scheme.placeholder(label, *n);
        if let Err(at) = self.lens.binary_search(&p.len()) {
            self.lens.insert(at, p.len());
        }
        self.back.insert(p.clone(), original.to_string());
        self.seen.insert(original.to_string(), p.clone());
        self.finder = std::sync::OnceLock::new();
        p
    }

    fn finder(&self) -> Option<&ValueFinder> {
        self.finder
            .get_or_init(|| ValueFinder::build(&self.seen).map(Arc::new))
            .as_deref()
    }
}

/// 账本里的每个原值，一遍扫过去全找出来（Aho-Corasick）：不拿每个值去整段文字里找一遍
/// —— 那是「值的个数 × 文字长度」，一个请求换了几万个值、回答有 1 MiB 时，打一次码要
/// 小半秒。
///
/// **每个位置只取在那儿结束的最长的那一个**：互相重叠的几处要合成一处（见 [`ValueSpans`]），
/// 而在同一个位置结束的几处，最长的那一处盖住了别的。不把每一处重叠的都列出来 —— 原值一个
/// 套一个（`\d{6,}` 认出来的一串串 1，长度 6 到 600）时，那是「位置 × 值的个数」，几百 KB 的
/// 正文就要上 G 的内存。
#[derive(Debug)]
struct ValueFinder {
    nfa: aho_corasick::nfa::contiguous::NFA,
    /// 第几个值换成哪个占位符
    placeholders: Vec<String>,
}

impl ValueFinder {
    fn build(seen: &HashMap<String, String>) -> Option<Self> {
        let (values, placeholders): (Vec<&str>, Vec<String>) = seen
            .iter()
            .filter(|(v, _)| !v.is_empty())
            .map(|(v, p)| (v.as_str(), p.clone()))
            .unzip();
        if values.is_empty() {
            return None;
        }
        let nfa = aho_corasick::nfa::contiguous::NFA::builder()
            .match_kind(aho_corasick::MatchKind::Standard)
            .build(&values)
            .ok()?;
        Some(Self { nfa, placeholders })
    }

    /// `text` 里原值盖住的地方，位置照 `to` 换算：`(区间, 第几个值)`。重叠的连成一段（挨着的
    /// 不连），一段记它头一处的值
    fn find_all(
        &self,
        text: &str,
        to: &dyn Fn(usize) -> usize,
        out: &mut Vec<(Range<usize>, usize)>,
    ) {
        use aho_corasick::Anchored;
        use aho_corasick::automaton::{Automaton, StateID};
        let Ok(mut sid) = self.nfa.start_state(Anchored::No) else {
            return;
        };
        // 每个状态上最长的那个值：用到才算，一段文字里走到的状态不比字多
        let mut longest: HashMap<StateID, (usize, usize)> = HashMap::new();
        let mut cur: Option<(usize, usize, usize)> = None;
        let push = |c: (usize, usize, usize), out: &mut Vec<(Range<usize>, usize)>| {
            out.push((to(c.0)..to(c.1), c.2));
        };
        for (i, &b) in text.as_bytes().iter().enumerate() {
            sid = self.nfa.next_state(Anchored::No, sid, b);
            if !self.nfa.is_match(sid) {
                continue;
            }
            let (len, v) = *longest.entry(sid).or_insert_with(|| {
                (0..self.nfa.match_len(sid))
                    .map(|k| {
                        let p = self.nfa.match_pattern(sid, k);
                        (self.nfa.pattern_len(p), p.as_usize())
                    })
                    .max_by_key(|&(len, _)| len)
                    .unwrap_or((0, 0))
            });
            if len == 0 {
                continue;
            }
            let (s, e) = ((i + 1).saturating_sub(len), i + 1);
            cur = match cur {
                Some((cs, ce, cv)) if s < ce => Some((cs.min(s), e, cv)),
                Some(c) => {
                    push(c, out);
                    Some((s, e, v))
                }
                None => Some((s, e, v)),
            };
        }
        if let Some(c) = cur {
            push(c, out);
        }
    }
}

/// 脱敏的结果。
#[derive(Debug, Clone)]
pub struct Redacted {
    /// 换过之后的文本。**没有命中时和原文逐字节相同**
    pub text: String,
    pub ledger: Ledger,
}

/// 把 `hits` 指的那些段换成占位符，**接着 `ledger` 已有的编号**。`text` 是 JSON 原文（请求体、
/// 一帧，[`crate::redact::rules::scan`] 找的那种）；解码过的正文用 [`apply_text`]。
///
/// 一次请求里有很多段文本（多条消息、WebSocket 的多帧）时，它们必须记在同一本
/// 账上：各起一本的话，第二段的 1 号会和第一段的撞车 —— 两个不同的值映射到
/// 同一个占位符，还原时必然给错一个。**那不是会不会发生的问题，是第二段只要
/// 命中一次就一定发生。**
///
/// `hits` 要**按起点排好、互不重叠**（[`crate::redact::rules::scan`] 给的就是）：区间指的
/// 都是 `text` 原文里的位置。
///
/// **一遍从前往后抄出新的一份**：原文的一段、一个占位符、原文的下一段……不在原文上就地
/// 换 —— 就地换一处，后面整段都要挪一次，一个请求里换几万处时是平方级的。
pub fn apply(text: &str, hits: &[Hit], ledger: Ledger) -> Redacted {
    apply_as(text, hits, ledger, Form::Json)
}

/// [`apply`]，`text` 是**解码过的正文**（[`crate::redact::rules::scan_text`]、`scan_plain`、
/// [`crate::redact::flow::hits_plain`] 找的那种）：换下来的值原样记账，不再解转义。
pub fn apply_text(text: &str, hits: &[Hit], ledger: Ledger) -> Redacted {
    apply_as(text, hits, ledger, Form::Text)
}

fn apply_as(text: &str, hits: &[Hit], mut ledger: Ledger, form: Form) -> Redacted {
    if hits.is_empty() {
        return Redacted {
            text: text.to_string(),
            ledger,
        };
    }
    debug_assert!(
        hits.windows(2).all(|w| w[0].bytes.end <= w[1].bytes.start),
        "hits must be sorted and disjoint"
    );
    // 编号按出现的先后发
    let default = ledger.scheme.label;
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for h in hits {
        let ph = ledger.issue(
            &text[h.bytes.clone()],
            h.label.as_deref().unwrap_or(default),
            form,
        );
        out.push_str(&text[at..h.bytes.start]);
        out.push_str(&ph);
        at = h.bytes.end;
    }
    out.push_str(&text[at..]);
    Redacted { text: out, ledger }
}

/// 只编号、不抄：[`apply`] 会给 `hits` 发的那些号，不写出换过的那一份。只要账本时用它
/// （拦截档下看一遍客户端原文，见 [`crate::redact::flow::look`]）—— 换过的那一份在每一跳
/// 发出去之前才写。`text` 是 JSON 原文。
pub(crate) fn number(text: &str, hits: &[Hit], mut ledger: Ledger) -> Ledger {
    let default = ledger.scheme.label;
    for h in hits {
        ledger.issue(
            &text[h.bytes.clone()],
            h.label.as_deref().unwrap_or(default),
            Form::Json,
        );
    }
    ledger
}

/// 扫 + 换，一步到位。`text` 是 JSON 请求体（见 [`crate::redact::rules::scan`]）。
pub fn redact(text: &str, rules: &RuleSet, ledger: Ledger) -> Redacted {
    let hits = crate::redact::rules::scan(text, rules);
    apply(text, &hits, ledger)
}

/// 扫 + 换一段**解码过的正文**（见 [`crate::redact::rules::scan_text`]）。
pub fn redact_text(text: &str, rules: &RuleSet, ledger: Ledger) -> Redacted {
    let hits = crate::redact::rules::scan_text(text, rules);
    apply_text(text, &hits, ledger)
}

/// 账本里记着的每个原值，在 `text` 里出现的每一处都换回它的占位符。
///
/// 回答里的占位符换回了原值之后，那份回答要存下来、写进日志时，按规则再找一遍不一定认得出
/// 原值：连接串的口令要有 `postgres://app:` 在前面才认得出，模型单独复述一遍口令，规则就
/// 认不出了；正则写成 `OPENAI_API_KEY=…` 的自定义规则，单独出现的那串值也一样。账本里记着
/// 原值本身，按字面找一遍，哪种都漏不掉。**落盘、写日志之前用**，不用在发出去的请求上（那里
/// 按规则找、按账本编号，见 [`crate::redact::flow`]）。
///
/// 怎么找见 [`value_spans`]。没有可换的时候原样借出去
pub fn hide_values<'a>(text: &'a str, ledger: &Ledger, form: Form) -> Cow<'a, str> {
    let found = value_spans(text, 0..text.len(), ledger, form);
    if found.is_empty() {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (r, placeholder) in found {
        out.push_str(text.get(at..r.start).unwrap_or_default());
        out.push_str(placeholder);
        at = r.end;
    }
    out.push_str(text.get(at..).unwrap_or_default());
    Cow::Owned(out)
}

/// `text[range]` 里账本记着的原值在哪儿、各换成哪个占位符：按先后、互不重叠，落在 `range`
/// 两头外面的那一截切掉。同一段文字要分几截问的，用 [`ValueSpans`]：只找一遍。
pub fn value_spans<'l>(
    text: &str,
    range: Range<usize>,
    ledger: &'l Ledger,
    form: Form,
) -> Vec<(Range<usize>, &'l str)> {
    ValueSpans::new(text, ledger, form).within(range).collect()
}

/// 一段文字里账本记着的原值都在哪儿、各换成哪个占位符：按先后、互不重叠。**整段只找一遍**，
/// 之后按区间取（[`Self::within`]）—— 安全日志一个请求要取上千段前后文，每一段都把整段
/// 文字从头找一遍，几 MB 的请求就要好几秒。
///
/// - `form` 是 `text` 的写法。**一整段合规的 JSON 只在字符串的引号里面找**：数值、`true`、
///   键之间的结构上的字换成占位符，存下来的正文、日志里的那一行就不是 JSON 了
///   （`"completion_tokens":4096` 里的 `4096`）。引号里面解开转义再找，`\n`、`\"`、`\uXXXX`、
///   `\/` 哪种写法都认得；字符串里又是一段 JSON 的（工具参数）、转义过几遍的，再往里解，
///   最多六层；
/// - 不是一整段合规 JSON 的（SSE 的几帧、纯文字的报错、解码过的正文）**哪儿都找**，数值也找
///   —— 宁可多换；引号里面照样解开转义再找一遍；
/// - **重叠的几处合成一处**，换成头一处的占位符：只换其中一处的话，另一处多出来的那一截就
///   原样留着（同出站脱敏的做法）；
/// - 已经写着的占位符里面不动，空的值不找。
#[derive(Debug, Default)]
pub struct ValueSpans<'l> {
    spans: Vec<(Range<usize>, &'l str)>,
}

impl<'l> ValueSpans<'l> {
    pub fn new(text: &str, ledger: &'l Ledger, form: Form) -> Self {
        let Some(finder) = (!text.is_empty()).then(|| ledger.finder()).flatten() else {
            return Self::default();
        };
        let mut found: Vec<(Range<usize>, usize)> = Vec::new();
        let whole_json = form == Form::Json && crate::redact::rules::is_json(text);
        if !whole_json {
            finder.find_all(text, &|i| i, &mut found);
        }
        if form == Form::Json {
            for span in crate::redact::rules::json_string_spans(text) {
                if let Some(content) = text.get(span.clone()).filter(|c| !c.is_empty()) {
                    // 不是一整段 JSON 的上面已经整段找过：没有转义的字符串不用再找
                    if whole_json || content.contains('\\') {
                        in_string(finder, content, &|i| span.start + i, &mut found);
                    }
                }
            }
        }
        if found.is_empty() {
            return Self::default();
        }
        found.retain(|(r, _)| {
            text.is_char_boundary(r.start) && text.is_char_boundary(r.end) && r.start < r.end
        });
        found.sort_by_key(|(r, _)| (r.start, std::cmp::Reverse(r.end)));
        let mut merged: Vec<(Range<usize>, usize)> = Vec::with_capacity(found.len());
        for (r, v) in found {
            match merged.last_mut() {
                Some((last, _)) if r.start < last.end => last.end = last.end.max(r.end),
                _ => merged.push((r, v)),
            }
        }
        // 已经写着的占位符里面不动：和它重叠的那一截让开，剩下的照样换 —— 整处不换的话，
        // 挨着占位符的那一截原值就原样留着（`<<` 是原值时的 `<<<<TW_SECRET_6>>`）。占位符的
        // 两头是 ASCII，切口落在字符边界上
        let ours: Vec<Range<usize>> = ledger
            .scheme
            .find_in(text)
            .into_iter()
            .map(|(r, _, _)| r)
            .collect();
        let mut spans = Vec::with_capacity(merged.len());
        for (r, v) in merged {
            let Some(p) = finder.placeholders.get(v).map(String::as_str) else {
                continue;
            };
            let mut from = r.start;
            let k = ours.partition_point(|o| o.end <= r.start);
            for o in ours[k..].iter().take_while(|o| o.start < r.end) {
                if o.start > from {
                    spans.push((from..o.start, p));
                }
                from = from.max(o.end);
            }
            if from < r.end {
                spans.push((from..r.end, p));
            }
        }
        Self { spans }
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// 和 `range` 重叠的那几处，切到 `range` 里面：一个原值跨在切口上，切下来的那一截也是它
    /// 的一部分，照样换
    pub fn within(
        &self,
        range: Range<usize>,
    ) -> impl Iterator<Item = (Range<usize>, &'l str)> + '_ {
        let k = self.spans.partition_point(|(r, _)| r.end <= range.start);
        self.spans[k..]
            .iter()
            .take_while(move |(r, _)| r.start < range.end)
            .map(move |(r, p)| (r.start.max(range.start)..r.end.min(range.end), *p))
            .filter(|(r, _)| r.start < r.end)
    }

    /// 跨在 `pos` 两边的那一处（`pos` 落在它中间）
    pub fn across(&self, pos: usize) -> Option<Range<usize>> {
        let k = self.spans.partition_point(|(r, _)| r.end <= pos);
        self.spans
            .get(k)
            .map(|(r, _)| r.clone())
            .filter(|r| r.start < pos && pos < r.end)
    }
}

/// 解开几层转义找原值，最多这么多层：一层是正文的字符串，再往里是工具参数那段 JSON 的、
/// 被再转义过的。每一层只在还有反斜杠时才解
const UNESCAPE_DEPTH: usize = 6;

/// 一个 JSON 字符串引号里面的一段（`content`，转义着的，`to` 把它里面的位置换回整段文字
/// 里的）：解开一层找原值；解开之后还有反斜杠的（一段 JSON、转义过两遍的字），整段再解一层，
/// 最多 [`UNESCAPE_DEPTH`] 层。
///
/// **一层一层往下解，只留着这一层**：位置的对照每解一层就并成「这一层 → `content`」的一张，
/// 上一层的字和对照随即放掉；哪一层什么都没解开（`\q` 这类不成样子的转义）就停。几 MB 满是
/// 反斜杠的字符串，每一层都留着的话要上百 MB
fn in_string(
    finder: &ValueFinder,
    content: &str,
    to: &dyn Fn(usize) -> usize,
    out: &mut Vec<(Range<usize>, usize)>,
) {
    if !content.contains('\\') || u32::try_from(content.len()).is_err() {
        // 没有转义：解开就是它自己
        finder.find_all(content, to, out);
        return;
    }
    let Some((mut plain, mut at)) = unescape_mapped(content) else {
        finder.find_all(content, to, out);
        return;
    };
    let back = |at: &[u32], i: usize| to(at.get(i).map_or(content.len(), |&p| p as usize));
    finder.find_all(&plain, &|i| back(&at, i), out);
    for _ in 1..UNESCAPE_DEPTH {
        if !plain.contains('\\') {
            break;
        }
        let Some((next, mut inner)) = unescape_mapped(&plain) else {
            break;
        };
        // 并成一张：这一层的位置 → 上一层的 → `content` 的
        for p in &mut inner {
            *p = at.get(*p as usize).copied().unwrap_or(content.len() as u32);
        }
        (plain, at) = (next, inner);
        finder.find_all(&plain, &|i| back(&at, i), out);
    }
}

/// JSON 字符串引号里面的一段解开一层转义：解出来的字，和它每个字节（含末尾）在 `s` 里从哪儿
/// 起。不成样子的转义（`\q`、半个 `\u`）照原样留着；落单的代理项解成 U+FFFD。一个转义都没
/// 解开的是 `None`（解出来就是它自己）。`s` 不超过 4 GiB（调用方查过）
fn unescape_mapped(s: &str) -> Option<(String, Vec<u32>)> {
    let mut out = String::with_capacity(s.len());
    let mut at: Vec<u32> = Vec::with_capacity(s.len() + 1);
    let mut changed = false;
    let push = |out: &mut String, at: &mut Vec<u32>, c: char, from: usize| {
        out.push(c);
        at.extend(std::iter::repeat_n(from as u32, c.len_utf8()));
    };
    let hex = |i: usize| {
        s.get(i..i + 4)
            .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|h| u32::from_str_radix(h, 16).ok())
    };
    let mut i = 0;
    while let Some(c) = s.get(i..).and_then(|r| r.chars().next()) {
        if c != '\\' {
            push(&mut out, &mut at, c, i);
            i += c.len_utf8();
            continue;
        }
        let simple = match s.as_bytes().get(i + 1) {
            Some(b'"') => Some('"'),
            Some(b'\\') => Some('\\'),
            Some(b'/') => Some('/'),
            Some(b'b') => Some('\u{8}'),
            Some(b'f') => Some('\u{c}'),
            Some(b'n') => Some('\n'),
            Some(b'r') => Some('\r'),
            Some(b't') => Some('\t'),
            _ => None,
        };
        if let Some(d) = simple {
            push(&mut out, &mut at, d, i);
            changed = true;
            i += 2;
            continue;
        }
        let Some(u) = (s.as_bytes().get(i + 1) == Some(&b'u'))
            .then(|| hex(i + 2))
            .flatten()
        else {
            push(&mut out, &mut at, '\\', i);
            i += 1;
            continue;
        };
        // 代理对写成两个 `\uXXXX`
        let pair = (0xD800..0xDC00).contains(&u)
            && s.get(i + 6..i + 8) == Some("\\u")
            && hex(i + 8).is_some_and(|l| (0xDC00..0xE000).contains(&l));
        let (c, len) = match (pair, hex(i + 8)) {
            (true, Some(l)) => (
                char::from_u32(0x10000 + ((u - 0xD800) << 10) + (l - 0xDC00)),
                12,
            ),
            _ => (char::from_u32(u), 6),
        };
        push(&mut out, &mut at, c.unwrap_or('\u{FFFD}'), i);
        changed = true;
        i += len;
    }
    at.push(s.len() as u32);
    changed.then_some((out, at))
}

/// 一次性还原一份 **JSON 原文**（非流式响应、WebSocket 的一帧、存下来的回答）：原值按它
/// 所在的那一层转义再放回去。私钥块里的换行回到 `\n`，和请求体里写的一样；一个带引号的
/// 值原样塞回去，整份 JSON 就坏了。
///
/// **按 JSON 的结构读一遍定层数**（见 [`crate::redact::nest`]）：正文字符串里的转义一次，
/// 工具参数那段 JSON 的字符串里的转义两次。不是 JSON 的文字一律转义一次。没有要转义的字的
/// 值原样放回去，没有占位符的地方一个字节都不动。
pub fn restore(text: &str, ledger: &Ledger) -> String {
    swap(text, ledger, Form::Json)
}

/// 一次性还原一段**解码过的正文**（插件交回来的字、解析好的一个字符串）：放回去的是原值
/// 本来的样子，换行就是换行。
pub fn restore_text(text: &str, ledger: &Ledger) -> String {
    swap(text, ledger, Form::Text)
}

fn swap(text: &str, ledger: &Ledger, form: Form) -> String {
    if ledger.is_empty() || !text.contains(ledger.scheme.open) {
        return text.to_string();
    }
    let lookup = |ph: &str| ledger.back.get(ph).map(String::as_str);
    let mut nest = Nest::default();
    let nest = match form {
        Form::Json => Some(&mut nest),
        Form::Text => None,
    };
    swap_with(text, ledger.scheme.open, &ledger.lens, lookup, nest)
}

/// `s` 转义 `times` 次
pub(crate) fn escaped_times(s: &str, times: usize) -> Cow<'_, str> {
    let mut out = Cow::Borrowed(s);
    for _ in 0..times {
        let next = match json_escaped(&out) {
            Cow::Owned(e) => e,
            // 这一遍没有要转义的，再转几遍也一样
            Cow::Borrowed(_) => break,
        };
        out = Cow::Owned(next);
    }
    out
}

/// 一遍扫过去，把 `text` 里写着的占位符换回原值。`lens` 是占位符有哪几种长度（从短到长），
/// `lookup` 按占位符查原值（解开转义之后的）。
///
/// `nest` 是 JSON 原文读到了哪儿（[`Form::Json`]）：每一处按它所在的层转义，读过的接着记在
/// 它上面 —— 流式还原的下一段从这里接着读。`None` 是解码过的正文，原值原样放。
///
/// **在每个占位符开头处按这几种长度各查一次表**，不拿账里的每个占位符去整段文字里找一遍：
/// 后者是「占位符个数 × 文字长度」，一个请求换了几万个值、回答又把它们念了一遍时，光
/// 还原就要几秒。换回来的原值不再参与匹配。
pub(crate) fn swap_with<'a>(
    text: &str,
    open: &str,
    lens: &[usize],
    lookup: impl Fn(&str) -> Option<&'a str>,
    mut nest: Option<&mut Nest>,
) -> String {
    let mut out = String::new();
    // 抄到了哪儿、从哪儿接着找下一个开头、读到了哪儿
    let (mut done, mut from, mut read) = (0, 0, 0);
    while let Some(i) = text[from..].find(open) {
        let at = from + i;
        let found = lens.iter().find_map(|&n| {
            let ph = text.get(at..at + n)?;
            lookup(ph).map(|original| (n, original))
        });
        match found {
            Some((n, original)) => {
                // 头一处才备下整段的地方：满是 `<<` 却一个占位符都没有的（C++ 代码）原样抄一份
                if done == 0 {
                    out.reserve(text.len());
                }
                out.push_str(&text[done..at]);
                match nest.as_deref_mut() {
                    Some(nest) => {
                        nest.advance(&text.as_bytes()[read..at]);
                        read = at;
                        out.push_str(&escaped_times(original, nest.level()));
                    }
                    None => out.push_str(original),
                }
                done = at + n;
                from = done;
            }
            // 开头那段是 ASCII，加一还落在字的边界上。`<<<TW_SECRET_1>>` 里的占位符从第二个
            // `<` 起
            None => from = at + 1,
        }
    }
    if let Some(nest) = nest {
        nest.advance(&text.as_bytes()[read..]);
    }
    if done == 0 {
        return text.to_string();
    }
    out.push_str(&text[done..]);
    out
}

#[cfg(test)]
mod tests {
    /// 账本里的原值照字面换回占位符：JSON 原文里转义过的写法也认，已经写着的占位符里面不动，
    /// 没有可换的原样借出
    #[test]
    fn values_in_the_ledger_are_hidden_as_written() {
        let pem = "line1\nline2";
        let text = format!("k=hunter2 pw {pem}");
        let hits = vec![
            Hit {
                bytes: 2..9,
                rule: crate::redact::rules::Rule::Custom("a".into()),
                label: None,
            },
            Hit {
                bytes: 13..text.len(),
                rule: crate::redact::rules::Rule::Custom("b".into()),
                label: None,
            },
        ];
        let ledger = apply_text(&text, &hits, Ledger::new(Scheme::SECRET)).ledger;
        // 回答复述了口令，规则单独认不出它
        let answer = r#"{"content":"The password hunter2 is wrong; key line1\nline2"}"#;
        assert_eq!(
            hide_values(answer, &ledger, Form::Json),
            r#"{"content":"The password <<TW_SECRET_1>> is wrong; key <<TW_SECRET_2>>"}"#
        );
        // 解码过的正文按原样找
        assert_eq!(
            hide_values("say hunter2 and line1\nline2", &ledger, Form::Text),
            "say <<TW_SECRET_1>> and <<TW_SECRET_2>>"
        );
        // 占位符里面不动，没有可换的原样借出
        assert!(matches!(
            hide_values("<<TW_SECRET_1>> clean", &ledger, Form::Json),
            Cow::Borrowed(_)
        ));
        let empty = Ledger::new(Scheme::SECRET);
        assert!(matches!(
            hide_values("hunter2", &empty, Form::Text),
            Cow::Borrowed(_)
        ));
    }

    /// 账本里记着这几个原值（解开转义之后的样子）
    fn ledger_of(values: &[&str]) -> Ledger {
        let text = values.join("\u{1}");
        let mut hits = Vec::new();
        let mut at = 0;
        for v in values {
            hits.push(Hit {
                bytes: at..at + v.len(),
                rule: crate::redact::rules::Rule::Custom("c".into()),
                label: None,
            });
            at += v.len() + 1;
        }
        apply_text(&text, &hits, Ledger::new(Scheme::SECRET)).ledger
    }

    /// JSON 原文只在字符串里换：数值、`true`、结构上的字不动，换过的还是 JSON
    #[test]
    fn hidden_values_stay_inside_json_strings() {
        let ledger = ledger_of(&["4096", "true"]);
        let text = r#"{"max_tokens":4096,"stream":true,"messages":[{"role":"user","content":"pin 4096 is true"}],"created":17604096123}"#;
        let out = hide_values(text, &ledger, Form::Json);
        assert_eq!(
            out,
            r#"{"max_tokens":4096,"stream":true,"messages":[{"role":"user","content":"pin <<TW_SECRET_1>> is <<TW_SECRET_2>>"}],"created":17604096123}"#
        );
        assert!(serde_json::from_str::<serde_json::Value>(&out).is_ok());
        // 不是一整段合规 JSON 的（SSE 的几帧、纯文字、不合规的 JSON）哪儿都换，数值也换：宁可多换
        let sse = "event: x\ndata: {\"n\":4096,\"t\":\"4096\"}\n\nplain 4096x and true";
        assert_eq!(
            hide_values(sse, &ledger, Form::Json),
            "event: x\ndata: {\"n\":<<TW_SECRET_1>>,\"t\":\"<<TW_SECRET_1>>\"}\n\nplain <<TW_SECRET_1>>x and <<TW_SECRET_2>>"
        );
        let ledger = ledger_of(&["620102198811110554", "84721937", "a\"b"]);
        assert_eq!(
            hide_values(
                "error: bad password 84721937 for 620102198811110554 a\"b",
                &ledger,
                Form::Json
            ),
            "error: bad password <<TW_SECRET_2>> for <<TW_SECRET_1>> <<TW_SECRET_3>>"
        );
        assert_eq!(
            hide_values("{\"p\":84721937-1}", &ledger, Form::Json),
            "{\"p\":<<TW_SECRET_2>>-1}",
            "不合规的 JSON 当一段文字"
        );
    }

    /// 一处原值和已经写着的占位符重叠：重叠的那一截让开，剩下的照样换
    #[test]
    fn a_value_touching_a_placeholder_is_masked_outside_it() {
        let ledger = ledger_of(&["<<"]);
        let p = ledger.placeholder_of("<<").unwrap().to_string();
        assert_eq!(
            hide_values("<<<<TW_SECRET_6>><<", &ledger, Form::Text),
            format!("{p}<<TW_SECRET_6>>{p}")
        );
    }

    /// 满是不成样子的转义（`\q`）的字符串解一层就停：解不开的不再往下解；里面的原值照样找得到
    #[test]
    fn strings_full_of_bogus_escapes_are_decoded_once() {
        let ledger = ledger_of(&["hunter2"]);
        let body = format!("{{\"x\":\"{} hunter2 \\\"pw\\\"\"}}", "\\q".repeat(100_000));
        let started = std::time::Instant::now();
        let out = hide_values(&body, &ledger, Form::Json);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(out.contains("<<TW_SECRET_1>>") && !out.contains("hunter2"));
        // 一层都没解开的字符串原样找
        let plain = format!("{{\"x\":\"{}hunter2\"}}", "\\q".repeat(10));
        assert_eq!(
            hide_values(&plain, &ledger, Form::Json),
            format!("{{\"x\":\"{}<<TW_SECRET_1>>\"}}", "\\q".repeat(10))
        );
    }

    /// 原值一个套一个（`\d{6,}` 认出来的一串串 1）：每个位置只取最长的，合成一段，不把每一处
    /// 重叠的都列出来
    #[test]
    fn nested_values_merge_into_one_region() {
        let values: Vec<String> = (6..40).map(|n| "1".repeat(n)).collect();
        let refs: Vec<&str> = values.iter().map(String::as_str).collect();
        let ledger = ledger_of(&refs);
        let text = format!("a {} b {} c", "1".repeat(39), "1".repeat(7));
        let out = hide_values(&text, &ledger, Form::Text);
        let ours = Scheme::SECRET.find_in(&out);
        assert_eq!(ours.len(), 2, "{out}");
        assert_eq!(out.matches(' ').count(), 4, "{out}");
        assert!(out.starts_with("a <<TW_") && out.ends_with(">> c"), "{out}");
        // 和逐个值、逐个位置找出来再合并的一样
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n.max(1) as u64) as usize
        };
        for _ in 0..2000 {
            let letters = ["a", "b", "ab", "ba", "aab"];
            let vals: Vec<String> = (0..1 + next(4))
                .map(|_| {
                    (0..1 + next(3))
                        .map(|_| letters[next(letters.len())])
                        .collect()
                })
                .collect();
            let refs: Vec<&str> = vals.iter().map(String::as_str).collect();
            let ledger = ledger_of(&refs);
            let text: String = (0..next(30)).map(|_| ["a", "b", " "][next(3)]).collect();
            let got: Vec<Range<usize>> = value_spans(&text, 0..text.len(), &ledger, Form::Text)
                .into_iter()
                .map(|(r, _)| r)
                .collect();
            let mut all: Vec<Range<usize>> = Vec::new();
            for v in &vals {
                for i in 0..text.len() {
                    if text[i..].starts_with(v.as_str()) {
                        all.push(i..i + v.len());
                    }
                }
            }
            all.sort_by_key(|r| (r.start, std::cmp::Reverse(r.end)));
            let mut want: Vec<Range<usize>> = Vec::new();
            for r in all {
                match want.last_mut() {
                    Some(last) if r.start < last.end => last.end = last.end.max(r.end),
                    _ => want.push(r),
                }
            }
            assert_eq!(got, want, "{text:?} {vals:?}");
        }
    }

    /// 字符串里各种写法、各层转义都认得：`\n`、`\"`、`\uXXXX`（含代理对）、`\/`，工具参数里
    /// 转义两次、再往里套几层的（最多五层都试）
    #[test]
    fn hidden_values_are_found_however_the_string_spells_them() {
        let v = "a\"b\nc\\d/中😀";
        let ledger = ledger_of(&[v]);
        let mut spelled = v.to_string();
        for depth in 1..=5 {
            spelled = serde_json::to_string(&spelled).unwrap();
            let body = serde_json::json!({ "x": format!("say {spelled} end") }).to_string();
            let out = hide_values(&body, &ledger, Form::Json);
            assert!(out.contains("<<TW_SECRET_1>>"), "第 {depth} 层：{out}");
            assert!(!out.contains('中'), "第 {depth} 层：{out}");
            assert!(serde_json::from_str::<serde_json::Value>(&out).is_ok());
        }
        // `\uXXXX` 写的（ensure_ascii 的客户端）、`\/` 写的
        let ledger = ledger_of(&["中文密码x", "a/b-secret"]);
        let text = r#"{"c":"中文密码x and a\/b-secret 😀"}"#;
        assert_eq!(
            hide_values(text, &ledger, Form::Json),
            r#"{"c":"<<TW_SECRET_1>> and <<TW_SECRET_2>> 😀"}"#
        );
    }

    /// 两个原值重叠：合成一处换掉，不留一截原样
    #[test]
    fn overlapping_hidden_values_leave_nothing_behind() {
        let ledger = ledger_of(&["abcX1", "X1defgh"]);
        let out = hide_values("zz abcX1defgh zz", &ledger, Form::Text);
        assert_eq!(out, "zz <<TW_SECRET_1>> zz");
        let out = hide_values(r#"{"t":"zz abcX1defgh zz"}"#, &ledger, Form::Json);
        assert_eq!(out, r#"{"t":"zz <<TW_SECRET_1>> zz"}"#);
        // 占位符里面照样不动
        let ledger = ledger_of(&["SECRET", "TW_", "1"]);
        assert_eq!(
            hide_values("a <<TW_SECRET_1>> b SECRET 1", &ledger, Form::Text),
            "a <<TW_SECRET_1>> b <<TW_SECRET_1>> <<TW_SECRET_3>>"
        );
    }

    use super::*;
    use crate::redact::rules::{BUILTINS, RuleSet};

    fn l() -> Ledger {
        Ledger::new(Scheme::SECRET)
    }

    const KEY: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn all() -> RuleSet {
        RuleSet::only(&BUILTINS.iter().map(|b| b.id).collect::<Vec<_>>())
    }

    #[test]
    fn nothing_to_redact_means_the_text_comes_back_byte_identical() {
        // **绝大多数请求一处都不命中**，而这个函数每个请求都要跑，
        // 所以它在「没命中」时必须是一次纯粹的拷贝。
        let t = "帮我看看这个 .env 文件\n";
        let r = redact(t, &all(), l());
        assert_eq!(r.text, t);
        assert!(r.ledger.is_empty());
        assert_eq!(restore(&r.text, &r.ledger), t);
    }

    #[test]
    fn a_key_becomes_a_placeholder_and_comes_back() {
        let t = format!("我的 key 是 {KEY}，帮我看看");
        let r = redact(&t, &all(), l());
        assert!(!r.text.contains(KEY), "{}", r.text);
        assert!(r.text.contains("<<TW_SECRET_1>>"), "{}", r.text);
        assert_eq!(restore(&r.text, &r.ledger), t);
    }

    #[test]
    fn a_key_at_the_start_of_a_line_is_replaced_and_the_body_stays_json() {
        // 请求体里的换行是 `\n` 两个字符。行首的 key 曾经和那个 `n` 粘在
        // 一起、认不出来 —— 而贴进对话的凭据文件里，key 偏偏常在行首
        let body = serde_json::json!({
            "messages": [{ "role": "user", "content": format!("keys:\n{KEY}\nthanks") }]
        })
        .to_string();
        let r = redact(&body, &all(), l());
        assert!(!r.text.contains(KEY), "{}", r.text);
        let v: serde_json::Value = serde_json::from_str(&r.text).expect("the body is still JSON");
        assert_eq!(
            v["messages"][0]["content"], "keys:\n<<TW_SECRET_1>>\nthanks",
            "{v}"
        );
        assert_eq!(restore(&r.text, &r.ledger), body);
    }

    #[test]
    fn the_same_value_twice_gets_the_same_placeholder() {
        // 一把 key 出现三次却换成三个不同的占位符，会让模型以为那是三个
        // 不同的东西 —— 而它可能正在帮你对比「这两处是不是同一把」。
        let t = format!("A={KEY}\nB={KEY}\nC={KEY}");
        let r = redact(&t, &all(), l());
        assert_eq!(r.ledger.len(), 1, "{:?}", r.ledger);
        assert_eq!(r.text.matches("<<TW_SECRET_1>>").count(), 3, "{}", r.text);
        assert_eq!(restore(&r.text, &r.ledger), t);
    }

    #[test]
    fn different_values_get_different_placeholders() {
        let other = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let t = format!("{KEY} 和 {other}");
        let r = redact(&t, &all(), l());
        assert_eq!(r.ledger.len(), 2);
        assert_eq!(restore(&r.text, &r.ledger), t);
    }

    #[test]
    fn several_hits_in_one_text_all_land_in_the_right_places() {
        // 从前往后替换的话，第一次替换就会让后面所有区间的偏移失效。
        // 那种错不会立刻炸，它会安静地切错一个字节。
        let t = format!("先 10.0.0.1 再 {KEY} 最后 postgres://u:pw@h/db");
        let r = redact(&t, &all(), l());
        assert_eq!(r.ledger.len(), 3, "{}", r.text);
        assert_eq!(restore(&r.text, &r.ledger), t);
        // 连接串只换了口令，host 还看得见
        assert!(r.text.contains("postgres://u:"), "{}", r.text);
        assert!(r.text.contains("@h/db"), "{}", r.text);
    }

    #[test]
    fn multibyte_text_around_a_hit_survives_intact() {
        // 这个项目已经被字节切片坑过三次。
        let t = format!("很长的一段中文说明，中间夹着 {KEY}，后面还有更多中文内容");
        let r = redact(&t, &all(), l());
        assert!(r.text.starts_with("很长的一段中文说明"), "{}", r.text);
        assert!(r.text.ends_with("后面还有更多中文内容"), "{}", r.text);
        assert_eq!(restore(&r.text, &r.ledger), t);
    }

    #[test]
    fn the_first_value_gets_the_first_number() {
        // 编号按出现的先后发 —— 从后往前换不该把号也倒过来
        let t = format!("{KEY} 然后 10.0.0.2");
        let r = redact(&t, &all(), l());
        assert!(r.text.starts_with("<<TW_SECRET_1>>"), "{}", r.text);
        assert!(r.text.ends_with("<<TW_SECRET_2>>"), "{}", r.text);
    }

    #[test]
    fn restoring_text_that_has_no_placeholder_is_a_passthrough() {
        let r = redact(&format!("k={KEY}"), &all(), l());
        assert_eq!(restore("模型说了点别的", &r.ledger), "模型说了点别的");
    }

    #[test]
    fn an_unknown_placeholder_is_left_alone() {
        // 模型自己编了一个 `<<TW_SECRET_9>>` 出来 —— 我们不认识它，
        // 那就原样交给客户端，而不是猜一个值填进去。
        let r = redact(&format!("k={KEY}"), &all(), l());
        assert_eq!(
            restore("这是 <<TW_SECRET_9>> 好吗", &r.ledger),
            "这是 <<TW_SECRET_9>> 好吗"
        );
    }
    #[test]
    fn a_second_frame_does_not_reuse_the_first_frames_placeholder_number() {
        // **第二帧只要命中一次就一定撞车** —— 两个不同的密钥映射到同一个
        // 占位符，还原时必然给错一个（WebSocket 那条路）
        let kinds = RuleSet::only(&["anthropic-api-key"]);
        let a = redact(
            "我的 key 是 sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAA",
            &kinds,
            l(),
        );
        let b = redact(
            "另一把是 sk-ant-api03-BBBBBBBBBBBBBBBBBBBBBBBB",
            &kinds,
            a.ledger.clone(),
        );
        assert!(a.text.contains("<<TW_SECRET_1>>"), "{}", a.text);
        assert!(
            b.text.contains("<<TW_SECRET_2>>"),
            "第二帧重用了 1 号：{}",
            b.text
        );
        // 一本账里两个都还原得回来
        let back = restore(&format!("{} / {}", a.text, b.text), &b.ledger);
        assert!(back.contains("AAAAAAAAAAAA"), "{back}");
        assert!(back.contains("BBBBBBBBBBBB"), "{back}");
    }

    #[test]
    fn the_same_secret_in_two_frames_keeps_one_number() {
        // 同一个值在一次连接里只该占一个编号 —— 否则模型会以为那是
        // 两个不同的东西
        let kinds = RuleSet::only(&["anthropic-api-key"]);
        let k = "sk-ant-api03-SAMESAMESAMESAMESAME1";
        let a = redact(&format!("第一次 {k}"), &kinds, l());
        let b = redact(&format!("第二次 {k}"), &kinds, a.ledger.clone());
        assert!(b.text.contains("<<TW_SECRET_1>>"), "{}", b.text);
        assert_eq!(b.ledger.len(), 1, "同一个值占了两个编号");
    }

    #[test]
    fn labeled_placeholders_count_per_label() {
        // `{{EMAIL_1}}`、`{{PHONE_1}}`，而不是 `{{PHONE_2}}` —— 后者让模型以为
        // 漏了一个
        let scheme = Scheme {
            open: "{{",
            close: "}}",
            label: "PII",
        };
        let rules = RuleSet::none()
            .with_labeled("email", r"[a-z]+@[a-z]+\.com", Some("EMAIL"))
            .unwrap()
            .with_labeled("phone", r"1[3-9]\d{9}", Some("PHONE"))
            .unwrap()
            .with_custom("other", r"ID-\d+")
            .unwrap();
        let t = "a@b.com 13800138000 c@d.com a@b.com ID-7";
        let hits = crate::redact::rules::scan_plain(t, &rules);
        let r = apply_text(t, &hits, Ledger::new(scheme));
        assert_eq!(
            r.text,
            "{{EMAIL_1}} {{PHONE_1}} {{EMAIL_2}} {{EMAIL_1}} {{PII_1}}"
        );
        assert_eq!(restore(&r.text, &r.ledger), t);
        let pairs: std::collections::HashMap<_, _> = r.ledger.replacements().collect();
        assert_eq!(pairs["c@d.com"], "{{EMAIL_2}}");
    }

    #[test]
    fn placeholders_already_in_the_text_keep_their_numbers_to_themselves() {
        // 存下来的请求里写着 1 号（拦截档下换过），重放时又找到一把新的：它拿 1 号的话，
        // 回显里的 1 号会被还原成这把新的，而那一处原本指的是另一样
        let t = format!("旧的 <<TW_SECRET_1>>，身份证 <<TW_ID_NUMBER_2>>，新的 {KEY}");
        let r = redact(&t, &all(), l().avoiding(&t));
        assert!(r.text.contains("新的 <<TW_SECRET_2>>"), "{}", r.text);
        assert!(r.text.contains("旧的 <<TW_SECRET_1>>"), "{}", r.text);
        // 让开的号不记账：回显里的 1 号原样留着，不被还原成任何东西
        assert_eq!(r.ledger.len(), 1);
        assert_eq!(
            restore("<<TW_SECRET_1>> 和 <<TW_SECRET_2>>", &r.ledger),
            format!("<<TW_SECRET_1>> 和 {KEY}")
        );
    }

    #[test]
    fn placeholders_are_found_where_they_are_written() {
        let t = "前 <<TW_SECRET_1>> 中 <<<TW_ID_NUMBER_12>>> 后 <<TW_x>> <<TW_SECRET_3";
        let found: Vec<_> = Scheme::SECRET
            .find_in(t)
            .into_iter()
            .map(|(at, label, n)| (&t[at], label, n))
            .collect();
        assert_eq!(
            found,
            vec![
                ("<<TW_SECRET_1>>", "TW_SECRET", 1),
                ("<<TW_ID_NUMBER_12>>", "TW_ID_NUMBER", 12),
            ]
        );
    }

    #[test]
    fn only_whole_placeholders_of_this_scheme_are_set_aside() {
        let taken = |t: &str| {
            let r = redact(KEY, &all(), l().avoiding(t));
            r.text
        };
        // 夹在别的字里的照样算：还原是按子串换的
        assert_eq!(taken("<<<TW_SECRET_4>>>"), "<<TW_SECRET_5>>");
        // 不是占位符的不让：没收尾的、号不是数字的、别的写法的、标签是空的
        for t in [
            "<<TW_SECRET_4",
            "<<TW_SECRET_x>>",
            "{{TW_SECRET_4}}",
            "<<_4>>",
            "<<TW SECRET_4>>",
            "<<TW_SECRET_99999999999999999999999>>",
        ] {
            assert_eq!(taken(t), "<<TW_SECRET_1>>", "{t}");
        }
        // 别的标签只让它自己的号
        assert_eq!(taken("<<TW_CARD_NUMBER_7>>"), "<<TW_SECRET_1>>");
    }

    /// 一个确定的伪随机序列
    fn rng(mut seed: u64) -> impl FnMut(usize) -> usize {
        move |n| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        }
    }

    /// 原来的做法：先从前往后发号，再从后往前就地换
    fn apply_in_place(text: &str, hits: &[Hit], mut ledger: Ledger) -> Redacted {
        let default = ledger.scheme.label;
        let placeholders: Vec<String> = hits
            .iter()
            .map(|h| {
                ledger.issue(
                    &text[h.bytes.clone()],
                    h.label.as_deref().unwrap_or(default),
                    Form::Json,
                )
            })
            .collect();
        let mut out = text.to_string();
        for (h, ph) in hits.iter().zip(&placeholders).rev() {
            out.replace_range(h.bytes.clone(), ph);
        }
        Redacted { text: out, ledger }
    }

    /// 原来的做法：账里的每个占位符在整段里找一遍、换一遍
    fn swap_each(text: &str, ledger: &Ledger) -> String {
        if ledger.is_empty() || !text.contains(ledger.scheme.open) {
            return text.to_string();
        }
        let mut out = text.to_string();
        for (ph, original) in ledger.table() {
            if out.contains(ph.as_str()) {
                out = out.replace(ph.as_str(), original);
            }
        }
        out
    }

    fn same_ledger(a: &Ledger, b: &Ledger) {
        assert_eq!(a.back, b.back);
        assert_eq!(a.seen, b.seen);
        assert_eq!(a.issued, b.issued);
    }

    /// 一遍抄出新的一份，和原来就地换，换出来的文字、发的号一模一样：有重复的值、带
    /// 自己标签的、接着一本已经发过号的账、多字节的字
    #[test]
    fn copying_once_writes_what_replacing_in_place_wrote() {
        let rules = all()
            .with_labeled("email", r"[a-z]+@[a-z]+\.com", Some("TW_EMAIL"))
            .unwrap();
        let pool = [
            KEY,
            "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "AKIAQRSTUVWXYZ234567",
            "10.0.0.7",
            "a@b.com",
            "c@d.com",
            "postgres://u:pw@h/db",
            "很长的中文",
            "plain",
        ];
        let mut next = rng(0x2545_f491_4f6c_dd1d);
        for round in 0..50 {
            let mut text = String::new();
            for _ in 0..next(60) {
                text.push_str(pool[next(pool.len())]);
                text.push_str(["", " ", "，", "\n"][next(4)]);
            }
            let hits = crate::redact::rules::scan(&text, &rules);
            let seed = if round % 2 == 0 {
                l()
            } else {
                // 接着一本发过号的账：同一个值复用旧号，新值往后编
                redact(&format!("{KEY} 10.0.0.9"), &all(), l()).ledger
            };
            let want = apply_in_place(&text, &hits, seed.clone());
            let got = apply(&text, &hits, seed);
            assert_eq!(got.text, want.text);
            same_ledger(&got.ledger, &want.ledger);
        }
    }

    /// 一遍扫过去，和账里的每个占位符各找一遍，换回来的一模一样：占位符挨着、夹在
    /// `<` 里、只写了半截、模型自己编的、几种长度和标签混着
    #[test]
    fn restoring_in_one_pass_puts_back_what_swapping_each_placeholder_did() {
        let mut ledger = l();
        for i in 0..120 {
            let label = ["TW_SECRET", "TW_ID_NUMBER", "TW_EMAIL"][i % 3];
            ledger.issue(&format!("value-{i}-密"), label, Form::Json);
        }
        let known: Vec<String> = ledger.table().keys().cloned().collect();
        let noise = [
            "",
            " ",
            "中文",
            "<",
            "<<",
            "<<<",
            ">>",
            "<<TW_SEC",
            "<<TW_SECRET_",
            "<<TW_SECRET_999>>",
            "<<TW_x>>",
            "std::cout << x << y;",
            "{{TW_SECRET_1}}",
        ];
        let mut next = rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..300 {
            let mut text = String::new();
            for _ in 0..next(40) {
                if next(2) == 0 {
                    text.push_str(&known[next(known.len())]);
                } else {
                    text.push_str(noise[next(noise.len())]);
                }
            }
            assert_eq!(restore(&text, &ledger), swap_each(&text, &ledger), "{text}");
            // 整包 JSON 的还原走同一遍
            let body = serde_json::json!({ "text": text }).to_string();
            let escaped = |s: &str| {
                let q = serde_json::to_string(s).unwrap();
                q[1..q.len() - 1].to_string()
            };
            let mut want = body.clone();
            for (ph, original) in ledger.table() {
                want = want.replace(ph.as_str(), &escaped(original));
            }
            assert_eq!(restore(&body, &ledger), want);
        }
        // 账是空的、文字里没有开头的：原样
        assert_eq!(restore("<<TW_SECRET_1>>", &l()), "<<TW_SECRET_1>>");
        assert_eq!(restore("没有占位符", &ledger), "没有占位符");
    }

    #[test]
    fn decoded_text_is_matched_as_written_and_restored_into_json_escaped() {
        // 正文上的 `password="x"`：截在引号处的话，换下来的只是 `password=`，
        // 真值原样发出去
        let rules = RuleSet::none()
            .with_labeled("pw", r#"password="[^"]*""#, Some("SECRET"))
            .unwrap();
        let text = r#"用 password="hunter2" 登录"#;
        let r = redact_text(text, &rules, l());
        assert_eq!(r.text, "用 <<SECRET_1>> 登录");
        // 回显在一份 JSON 里：放回去要转义，否则整份 JSON 就坏了
        let body = serde_json::json!({ "text": r.text }).to_string();
        let back: serde_json::Value =
            serde_json::from_str(&restore(&body, &r.ledger)).expect("still JSON");
        assert_eq!(back["text"], text);
        // 放回解码过的正文里是原样：引号就是引号
        assert_eq!(restore_text(&r.text, &r.ledger), text);
    }

    const PEM: &str = "-----BEGIN RSA PRIVATE KEY-----\nMIIBOgIBAAJBAKj34GkxFhD90vcNLYLInFEX6Ppy1tPf9Cnzj4p4WGeKLs1Pt8Qu\n-----END RSA PRIVATE KEY-----";

    #[test]
    fn a_value_found_in_json_is_written_down_decoded_and_put_back_in_the_form_asked_for() {
        // 请求体里私钥块的换行是 `\n` 两个字符。原样记账的话，放进解码过的正文是一个反斜杠
        // 和一个 n，放回 JSON 原文时再转义一次就成了 `\\n`
        let body = serde_json::json!({ "content": format!("钥匙：{PEM}") }).to_string();
        assert!(body.contains("KEY-----\\nMIIB"), "{body}");
        let r = redact(&body, &all(), l());
        assert_eq!(r.ledger.len(), 1, "{}", r.text);
        assert_eq!(r.ledger.table()["<<TW_SECRET_1>>"], PEM);
        assert_eq!(r.ledger.placeholder_of(PEM), Some("<<TW_SECRET_1>>"));
        // 整份 JSON：转义一次，和发来的逐字节相同
        assert_eq!(restore(&r.text, &r.ledger), body);
        // 解码过的正文：真的换行
        assert_eq!(
            restore_text("见 <<TW_SECRET_1>>", &r.ledger),
            format!("见 {PEM}")
        );
    }

    #[test]
    fn an_escaped_password_comes_back_escaped_once() {
        // 连接串的口令里写着 `\\`：解开是一个反斜杠
        let body = r#"{"url":"postgres://app:p\\w@db/x"}"#;
        let r = redact(body, &all(), l());
        assert_eq!(r.text, r#"{"url":"postgres://app:<<TW_SECRET_1>>@db/x"}"#);
        assert_eq!(r.ledger.placeholder_of(r"p\w"), Some("<<TW_SECRET_1>>"));
        assert_eq!(restore(&r.text, &r.ledger), body);
        let v: serde_json::Value = serde_json::from_str(&restore(&r.text, &r.ledger)).unwrap();
        assert_eq!(v["url"], r"postgres://app:p\w@db/x");
        assert_eq!(restore_text("<<TW_SECRET_1>>", &r.ledger), r"p\w");
    }

    #[test]
    fn a_backslash_in_decoded_text_is_not_taken_for_an_escape() {
        // 正文里写着的 `\n` 是一个反斜杠和一个 n，不是换行：再解一次，它就变成了换行
        let rules = RuleSet::none()
            .with_labeled("pw", r"pw=\S+", Some("SECRET"))
            .unwrap();
        let text = r#"登录用 pw=a\nb\"c 就行"#;
        let r = redact_text(text, &rules, l());
        assert_eq!(r.text, "登录用 <<SECRET_1>> 就行");
        assert_eq!(
            r.ledger.placeholder_of(r#"pw=a\nb\"c"#),
            Some("<<SECRET_1>>")
        );
        assert_eq!(restore_text(&r.text, &r.ledger), text);
        let body = serde_json::json!({ "text": r.text }).to_string();
        let back: serde_json::Value = serde_json::from_str(&restore(&body, &r.ledger)).unwrap();
        assert_eq!(back["text"], text);
    }

    #[test]
    fn the_same_value_in_json_and_in_decoded_text_shares_one_number() {
        // 请求体里认出的私钥块，插件那一层在解开的正文里又看到它：还是同一个号
        let body = serde_json::json!({ "content": PEM }).to_string();
        let a = redact(&body, &all(), l());
        let b = redact_text(&format!("又是 {PEM}"), &all(), a.ledger);
        assert_eq!(b.text, "又是 <<TW_SECRET_1>>");
        assert_eq!(b.ledger.len(), 1);
    }

    #[test]
    fn json_escaping_lends_out_what_needs_none() {
        assert!(matches!(json_escaped("plain 中文"), Cow::Borrowed(_)));
        assert_eq!(json_escaped("a\"b\\c\nd\u{1}"), r#"a\"b\\c\nd\u0001"#);
    }

    /// 一份请求体里换下来的私钥块的账
    fn pem_ledger() -> Ledger {
        let body = serde_json::json!({ "content": PEM }).to_string();
        redact(&body, &all(), l()).ledger
    }

    fn arg_of(args: &str, key: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(args).expect("the arguments are JSON");
        v[key].as_str().unwrap().to_string()
    }

    #[test]
    fn a_placeholder_in_tool_arguments_inside_a_whole_answer_is_escaped_twice() {
        // 参数那段 JSON 本身是一个字符串：解开外面一层是参数，再解开一层才是值。只转义一次，
        // 客户端解出来的参数里夹着一个真的换行
        let l = pem_ledger();
        let chat = serde_json::json!({"choices": [{"message": {
            "content": "钥匙：<<TW_SECRET_1>>",
            "tool_calls": [{"function": {"name": "write", "arguments": "{\"pem\":\"<<TW_SECRET_1>>\"}"}}],
        }}]})
        .to_string();
        let v: serde_json::Value = serde_json::from_str(&restore(&chat, &l)).expect("still JSON");
        let m = &v["choices"][0]["message"];
        assert_eq!(m["content"], format!("钥匙：{PEM}"));
        let args = m["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        assert_eq!(arg_of(args, "pem"), PEM);

        // Responses 的 function_call 项、Converse 的 toolUse.input 一样；Anthropic 的 input
        // 是对象，一层
        let other = serde_json::json!({
            "output": [{"type": "function_call", "arguments": "{\"pem\":\"<<TW_SECRET_1>>\"}"}],
            "delta": {"toolUse": {"input": "{\"pem\":\"<<TW_SECRET_1>>\"}"}},
            "content": [{"type": "tool_use", "input": {"pem": "<<TW_SECRET_1>>", "arguments": "照 <<TW_SECRET_1>>"}}],
        })
        .to_string();
        let v: serde_json::Value = serde_json::from_str(&restore(&other, &l)).expect("still JSON");
        assert_eq!(
            arg_of(v["output"][0]["arguments"].as_str().unwrap(), "pem"),
            PEM
        );
        assert_eq!(
            arg_of(v["delta"]["toolUse"]["input"].as_str().unwrap(), "pem"),
            PEM
        );
        assert_eq!(v["content"][0]["input"]["pem"], PEM);
        // 工具自己的参数里叫 `arguments` 的字段放的是一句话
        assert_eq!(v["content"][0]["input"]["arguments"], format!("照 {PEM}"));
    }

    #[test]
    fn only_the_placeholders_move_in_a_whole_answer() {
        // 别的字节一个不动：`\u00e9` 还是 `\u00e9`，空白、键的先后照旧
        let l = pem_ledger();
        let bs = '\\';
        let body = format!("{{ \"b\": \"{bs}u00e9 <<TW_SECRET_1>>\",\n  \"a\": 1 }}");
        let want = format!(
            "{{ \"b\": \"{bs}u00e9 {}\",\n  \"a\": 1 }}",
            json_escaped(PEM)
        );
        assert_eq!(restore(&body, &l), want);
        // 没有占位符的原样
        let none = "{ \"arguments\": \"{}\" }";
        assert_eq!(restore(none, &l), none);
        // 不是 JSON 的文字：转义一次
        assert_eq!(
            restore("你说的是 <<TW_SECRET_1>>", &l),
            format!("你说的是 {}", json_escaped(PEM))
        );
    }

    #[test]
    fn a_tools_own_arguments_field_holding_json_is_one_level_deeper_again() {
        // 工具的参数里又有个 `arguments`，放的是一段 JSON：最里面的值转义三次，不然客户端
        // 一层层解到底，拿到的字符串里夹着真的换行
        let l = pem_ledger();
        let args = serde_json::json!({
            "tool": "write_file",
            "arguments": serde_json::json!({ "content": "<<TW_SECRET_1>>" }).to_string(),
        })
        .to_string();
        let chat = serde_json::json!({"choices": [{"message": {
            "tool_calls": [{"function": {"name": "call", "arguments": args}}],
        }}]})
        .to_string();
        let v: serde_json::Value = serde_json::from_str(&restore(&chat, &l)).expect("still JSON");
        let args = v["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        let inner = arg_of(args, "arguments");
        assert_eq!(arg_of(&inner, "content"), PEM);
    }
}
