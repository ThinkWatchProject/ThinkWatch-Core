//! 把敏感值换成占位符，并记住怎么换回来。
//!
//! **可逆替换，不是删除。**删掉的话模型看到半截连接串反而会瞎猜 ——
//! 而「瞎猜」在一个正帮你调试 `.env` 的助手身上，比看不见更糟。

use std::collections::HashMap;
use std::ops::Range;

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

/// 一次脱敏留下的账本。
///
/// **同一个值在一次请求里只占一个编号。**一把 key 出现三次却换成三个
/// 不同的占位符，会让模型以为那是三个不同的东西 —— 而它可能正在帮你
/// 对比「这两处的 key 是不是同一把」。
///
/// **编号按标签各数各的**：`{{EMAIL_1}}`、`{{PHONE_1}}`，而不是
/// `{{EMAIL_1}}`、`{{PHONE_2}}` —— 后者让模型以为漏了一个。
///
/// 换了哪些、各几处不记在这里：那是 [`crate::redact::rules::findings`] 从命中里
/// 算出来的，观察档（不换）和拦截档（换）报的是同一份。
#[derive(Debug, Clone)]
pub struct Ledger {
    scheme: Scheme,
    /// 占位符 → 原值
    back: HashMap<String, String>,
    /// 原值 → 占位符，用来复用编号
    seen: HashMap<String, String>,
    /// 每个标签发到几号了
    issued: HashMap<String, usize>,
    /// 发出去的占位符有哪几种长度，从短到长。还原时在每个开头处按它们各查一次表（见
    /// [`restore`]）；标签就那几个、号的位数也就几种，所以这里只有寥寥几个数
    lens: Vec<usize>,
}

impl Ledger {
    pub fn new(scheme: Scheme) -> Self {
        Self {
            scheme,
            back: HashMap::new(),
            seen: HashMap::new(),
            issued: HashMap::new(),
            lens: Vec::new(),
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
    /// 占位符 → 原值。流式还原要拿它。
    pub fn table(&self) -> &HashMap<String, String> {
        &self.back
    }
    /// 发出去的占位符有哪几种长度，从短到长
    pub(crate) fn lens(&self) -> &[usize] {
        &self.lens
    }
    /// 原值 → 占位符。在一处找到的值要换到别处去时用它（企业版在解码后的
    /// 请求上找，再换进原样转发的那一份）。
    pub fn replacements(&self) -> impl Iterator<Item = (&str, &str)> {
        self.seen.iter().map(|(o, p)| (o.as_str(), p.as_str()))
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

    /// 这个值的占位符，头一次见就发一个新号。
    fn issue(&mut self, original: &str, label: &str) -> String {
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
        p
    }
}

/// 脱敏的结果。
#[derive(Debug, Clone)]
pub struct Redacted {
    /// 换过之后的文本。**没有命中时和原文逐字节相同**
    pub text: String,
    pub ledger: Ledger,
}

/// 把 `hits` 指的那些段换成占位符，**接着 `ledger` 已有的编号**。
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
pub fn apply(text: &str, hits: &[Hit], mut ledger: Ledger) -> Redacted {
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
/// 发出去之前才写。
pub(crate) fn number(text: &str, hits: &[Hit], mut ledger: Ledger) -> Ledger {
    let default = ledger.scheme.label;
    for h in hits {
        ledger.issue(
            &text[h.bytes.clone()],
            h.label.as_deref().unwrap_or(default),
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
    apply(text, &hits, ledger)
}

/// 一次性还原（非流式响应、错误信息）。
///
/// 按 [`crate::redact::rules::scan`] / `scan_plain` 换下来的值里从来没有引号和
/// 反斜杠，原样放回 JSON 字符串里也还是合法的 JSON。按 `scan_text` 换的可能有，
/// 整包 JSON 用 [`restore_json`]。
pub fn restore(text: &str, ledger: &Ledger) -> String {
    swap(text, ledger, |s| s.to_string())
}

/// 一次性还原一份 **JSON 文本**：原值按 JSON 字符串转义之后再放回去 —— 一个
/// 带引号的值原样塞回去，整份 JSON 就坏了。
pub fn restore_json(text: &str, ledger: &Ledger) -> String {
    swap(text, ledger, |s| {
        let quoted = serde_json::to_string(s).unwrap_or_default();
        quoted[1..quoted.len().saturating_sub(1)].to_string()
    })
}

fn swap(text: &str, ledger: &Ledger, put: impl Fn(&str) -> String) -> String {
    if ledger.is_empty() {
        return text.to_string();
    }
    let lookup = |ph: &str| ledger.back.get(ph).map(String::as_str);
    swap_with(text, ledger.scheme.open, &ledger.lens, lookup, put)
}

/// 一遍扫过去，把 `text` 里写着的占位符换回原值。`lens` 是占位符有哪几种长度（从短到长），
/// `lookup` 按占位符查原值。
///
/// **在每个占位符开头处按这几种长度各查一次表**，不拿账里的每个占位符去整段文字里找一遍：
/// 后者是「占位符个数 × 文字长度」，一个请求换了几万个值、回答又把它们念了一遍时，光
/// 还原就要几秒。换回来的原值不再参与匹配。
pub(crate) fn swap_with<'a>(
    text: &str,
    open: &str,
    lens: &[usize],
    lookup: impl Fn(&str) -> Option<&'a str>,
    put: impl Fn(&str) -> String,
) -> String {
    let mut out = String::new();
    // 抄到了哪儿、从哪儿接着找下一个开头
    let (mut done, mut from) = (0, 0);
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
                out.push_str(&put(original));
                done = at + n;
                from = done;
            }
            // 开头那段是 ASCII，加一还落在字的边界上。`<<<TW_SECRET_1>>` 里的占位符从第二个
            // `<` 起
            None => from = at + 1,
        }
    }
    if done == 0 {
        return text.to_string();
    }
    out.push_str(&text[done..]);
    out
}

#[cfg(test)]
mod tests {
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
        let r = apply(t, &hits, Ledger::new(scheme));
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
            ledger.issue(&format!("value-{i}-密"), label);
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
            assert_eq!(restore_json(&body, &ledger), want);
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
            serde_json::from_str(&restore_json(&body, &r.ledger)).expect("still JSON");
        assert_eq!(back["text"], text);
    }
}
