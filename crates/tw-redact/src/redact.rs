//! 把凭据换成占位符，并记住怎么换回来（DESIGN.md §5.1）。
//!
//! **可逆替换，不是删除。**删掉的话模型看到半截连接串反而会瞎猜 ——
//! 而「瞎猜」在一个正帮你调试 `.env` 的助手身上，比看不见更糟。

use std::collections::HashMap;

use crate::rules::{Hit, Kind};

/// 占位符长什么样：`<<TW_SECRET_1>>`。
pub const OPEN: &str = "<<TW_SECRET_";
pub const CLOSE: &str = ">>";

pub fn placeholder(n: usize) -> String {
    format!("{OPEN}{n}{CLOSE}")
}

/// 一次脱敏留下的账本。
///
/// **同一个值在一次请求里只占一个编号。**一把 key 出现三次却换成三个
/// 不同的占位符，会让模型以为那是三个不同的东西 —— 而它可能正在帮你
/// 对比「这两处的 key 是不是同一把」。
#[derive(Debug, Clone, Default)]
pub struct Ledger {
    /// 占位符 → 原值
    back: HashMap<String, String>,
    /// 原值 → 占位符，用来复用编号
    seen: HashMap<String, String>,
    /// 换了哪些、各几处。**界面上要说得出来**（§5.1：看不见的安全功能
    /// 会被用户关掉，因为他们会怀疑是脱敏搞坏了功能）
    pub counts: Vec<(Kind, &'static str, usize)>,
}

impl Ledger {
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
    fn note(&mut self, kind: Kind, what: &'static str) {
        match self
            .counts
            .iter_mut()
            .find(|(k, w, _)| *k == kind && *w == what)
        {
            Some((_, _, n)) => *n += 1,
            None => self.counts.push((kind, what, 1)),
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

/// 把 `hits` 指的那些段换成占位符。
///
/// **从后往前替换。**从前往后的话，第一次替换就会让后面所有区间的偏移
/// 失效 —— 而那种错不会立刻炸，它会安静地切错一个字节，然后你拿到一份
/// 坏掉的 JSON。
pub fn apply(text: &str, hits: &[Hit]) -> Redacted {
    apply_into(text, hits, Ledger::default())
}

/// 同上，但**接着一本已有的账本编号**。
///
/// WebSocket 那条路要用它（§3.6）：一次连接里有很多帧，而每帧各起一本
/// 账的话，第二帧的 `<<TW_SECRET_1>>` 会和第一帧的撞车 —— 两个不同的
/// 密钥映射到同一个占位符，还原时必然给错一个。**那不是会不会发生的
/// 问题，是第二帧只要命中一次就一定发生。**
pub fn apply_into(text: &str, hits: &[Hit], mut ledger: Ledger) -> Redacted {
    if hits.is_empty() {
        return Redacted {
            text: text.to_string(),
            ledger,
        };
    }
    // 这一次新换了什么，单独记 —— 事件里要报的是「这一帧换了什么」，
    // 不是「这条连接至今换了什么」
    let before = ledger.counts.len();
    let _ = before;
    let mut out = text.to_string();
    for h in hits.iter().rev() {
        let original = &text[h.bytes.clone()];
        let ph = match ledger.seen.get(original) {
            Some(p) => p.clone(),
            None => {
                let p = placeholder(ledger.back.len() + 1);
                ledger.back.insert(p.clone(), original.to_string());
                ledger.seen.insert(original.to_string(), p.clone());
                p
            }
        };
        ledger.note(h.kind, h.what);
        out.replace_range(h.bytes.clone(), &ph);
    }
    ledger.counts.sort_by_key(|(k, w, _)| (*k, *w));
    Redacted { text: out, ledger }
}

/// 扫 + 换，一步到位。
pub fn redact(text: &str, kinds: &[Kind]) -> Redacted {
    let hits = crate::rules::scan(text, kinds);
    apply(text, &hits)
}

/// 扫 + 换，接着一本已有的账本编号。见 [`apply_into`]。
pub fn redact_into(text: &str, kinds: &[Kind], ledger: Ledger) -> Redacted {
    let hits = crate::rules::scan(text, kinds);
    apply_into(text, &hits, ledger)
}

/// 一次性还原（非流式响应、错误信息）。
pub fn restore(text: &str, ledger: &Ledger) -> String {
    if ledger.is_empty() || !text.contains(OPEN) {
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

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn all() -> Vec<Kind> {
        Kind::all().to_vec()
    }

    #[test]
    fn nothing_to_redact_means_the_text_comes_back_byte_identical() {
        // **走官方端点不该脱敏**（§5.1），那条路径上这个函数每次都要跑，
        // 所以它在「没命中」时必须是一次纯粹的拷贝。
        let t = "帮我看看这个 .env 文件\n";
        let r = redact(t, &all());
        assert_eq!(r.text, t);
        assert!(r.ledger.is_empty());
        assert_eq!(restore(&r.text, &r.ledger), t);
    }

    #[test]
    fn a_key_becomes_a_placeholder_and_comes_back() {
        let t = format!("我的 key 是 {KEY}，帮我看看");
        let r = redact(&t, &all());
        assert!(!r.text.contains(KEY), "{}", r.text);
        assert!(r.text.contains("<<TW_SECRET_1>>"), "{}", r.text);
        assert_eq!(restore(&r.text, &r.ledger), t);
    }

    #[test]
    fn the_same_value_twice_gets_the_same_placeholder() {
        // 一把 key 出现三次却换成三个不同的占位符，会让模型以为那是三个
        // 不同的东西 —— 而它可能正在帮你对比「这两处是不是同一把」。
        let t = format!("A={KEY}\nB={KEY}\nC={KEY}");
        let r = redact(&t, &all());
        assert_eq!(r.ledger.len(), 1, "{:?}", r.ledger);
        assert_eq!(r.text.matches("<<TW_SECRET_1>>").count(), 3, "{}", r.text);
        assert_eq!(restore(&r.text, &r.ledger), t);
    }

    #[test]
    fn different_values_get_different_placeholders() {
        let other = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let t = format!("{KEY} 和 {other}");
        let r = redact(&t, &all());
        assert_eq!(r.ledger.len(), 2);
        assert_eq!(restore(&r.text, &r.ledger), t);
    }

    #[test]
    fn several_hits_in_one_text_all_land_in_the_right_places() {
        // 从前往后替换的话，第一次替换就会让后面所有区间的偏移失效。
        // 那种错不会立刻炸，它会安静地切错一个字节。
        let t = format!("先 10.0.0.1 再 {KEY} 最后 postgres://u:pw@h/db");
        let r = redact(&t, &all());
        assert_eq!(r.ledger.len(), 3, "{}", r.text);
        assert_eq!(restore(&r.text, &r.ledger), t);
        // 连接串只换了口令，host 还看得见
        assert!(r.text.contains("postgres://u:"), "{}", r.text);
        assert!(r.text.contains("@h/db"), "{}", r.text);
    }

    #[test]
    fn multibyte_text_around_a_hit_survives_intact() {
        // 这个项目已经被字节切片坑过三次（§9.7）。
        let t = format!("很长的一段中文说明，中间夹着 {KEY}，后面还有更多中文内容");
        let r = redact(&t, &all());
        assert!(r.text.starts_with("很长的一段中文说明"), "{}", r.text);
        assert!(r.text.ends_with("后面还有更多中文内容"), "{}", r.text);
        assert_eq!(restore(&r.text, &r.ledger), t);
    }

    #[test]
    fn the_ledger_can_say_what_it_replaced_without_showing_it() {
        // **界面上必须能看到脱敏发生了什么**（§5.1）—— 看不见的安全功能
        // 会被用户关掉，因为他们会怀疑是脱敏搞坏了功能。
        let t = format!("{KEY} 和 10.0.0.1 和 10.0.0.2");
        let r = redact(&t, &all());
        let summary = format!("{:?}", r.ledger.counts);
        assert!(summary.contains("Anthropic API key"), "{summary}");
        assert!(summary.contains("内网地址"), "{summary}");
        assert!(
            !summary.contains("sk-ant-api03-AAAA"),
            "计数里带出了原值：{summary}"
        );
        let internal = r
            .ledger
            .counts
            .iter()
            .find(|(k, _, _)| *k == Kind::Internal)
            .unwrap();
        assert_eq!(internal.2, 2);
    }

    #[test]
    fn restoring_text_that_has_no_placeholder_is_a_passthrough() {
        let r = redact(&format!("k={KEY}"), &all());
        assert_eq!(restore("模型说了点别的", &r.ledger), "模型说了点别的");
    }

    #[test]
    fn an_unknown_placeholder_is_left_alone() {
        // 模型自己编了一个 `<<TW_SECRET_9>>` 出来 —— 我们不认识它，
        // 那就原样交给客户端，而不是猜一个值填进去。
        let r = redact(&format!("k={KEY}"), &all());
        assert_eq!(
            restore("这是 <<TW_SECRET_9>> 好吗", &r.ledger),
            "这是 <<TW_SECRET_9>> 好吗"
        );
    }
    #[test]
    fn a_second_frame_does_not_reuse_the_first_frames_placeholder_number() {
        // **第二帧只要命中一次就一定撞车** —— 两个不同的密钥映射到同一个
        // 占位符，还原时必然给错一个（WebSocket 那条路，§3.6）
        let kinds = [Kind::ApiKeys];
        let a = redact("我的 key 是 sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAA", &kinds);
        let b = redact_into(
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
        let kinds = [Kind::ApiKeys];
        let k = "sk-ant-api03-SAMESAMESAMESAMESAME1";
        let a = redact(&format!("第一次 {k}"), &kinds);
        let b = redact_into(&format!("第二次 {k}"), &kinds, a.ledger.clone());
        assert!(b.text.contains("<<TW_SECRET_1>>"), "{}", b.text);
        assert_eq!(b.ledger.len(), 1, "同一个值占了两个编号");
    }
}
