//! 流式还原：占位符被切在两个 chunk 中间时怎么办。
//!
//! **扣住尾部那段「还可能长成占位符」的内容不发**，等下一个 chunk 到了再判断。
//!
//! # 判据：必须是账本里某个占位符的前缀
//!
//! 企业版最早的判据是「最右边那个 `{{` 之后没有 `}}` 就全扣住」。照搬到
//! `<<TW_SECRET_n>>` 上会**当场坏掉**：
//!
//! ```text
//! std::cout << x << std::endl;    ← C++ 满屏都是
//! cat <<EOF                       ← heredoc 同样常见
//! ```
//!
//! `<<` 之后的所有内容会一直扣着，直到某处出现 `>>` 或者流结束 —— 用户看到的
//! 是**响应卡住**。`{{` 也一样：Jinja、Handlebars、Vue 的模板里到处都是。
//!
//! 所以扣住的那一段必须是**这次真的发出去过的某个占位符**的一个严格前缀。
//! 只有账本里的占位符才需要还原，别的都不必等：`<< x` 立刻放行，一个模型自己
//! 编出来的 `<<TW_SECRET_9>>` 也原样过去。扣住的长度因此天然封顶在最长的那个
//! 占位符上 —— 几十个字节。

use std::sync::Arc;

use crate::redact::replace::Ledger;

/// 一条流上的还原器。
///
/// **`process()` 的输出拼起来再接上 `flush()`，等于把整段内容一次性
/// 还原的结果。**顺序和内容都不变，唯一的差别是有些字节晚几毫秒发出去。
pub struct Restorer {
    book: Arc<Book>,
    /// 占位符开头的第一个字节。先按它筛，再比前缀
    lead: u8,
    open: &'static str,
    buffer: String,
}

/// 还原要查的那本账，建一次、一条流里的几路共用（见 [`Restorer::fresh`]）。
///
/// **按占位符排好序**：一段尾巴是不是某个占位符的前缀、一个占位符的原值是什么，都是二分
/// 查一次，不拿账里的每个占位符挨个比 —— 一个请求换了几万个值时，后者在每个 chunk 上
/// 都要比几万次。
struct Book {
    /// (占位符, 原值)，按占位符的字节序排
    entries: Vec<(String, String)>,
    /// 占位符有哪几种长度，从短到长
    lens: Vec<usize>,
    /// 最长的占位符有多长：扣住的永远不会比它更多
    longest: usize,
}

impl Book {
    fn of(ledger: &Ledger) -> Self {
        let mut entries: Vec<(String, String)> = ledger
            .table()
            .iter()
            .map(|(p, o)| (p.clone(), o.clone()))
            .collect();
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let lens = ledger.lens().to_vec();
        Self {
            longest: lens.last().copied().unwrap_or(0),
            entries,
            lens,
        }
    }

    /// 第一个不小于 `s` 的占位符在哪儿
    fn seek(&self, s: &[u8]) -> usize {
        self.entries.partition_point(|(p, _)| p.as_bytes() < s)
    }

    fn original(&self, placeholder: &str) -> Option<&str> {
        let (p, o) = self.entries.get(self.seek(placeholder.as_bytes()))?;
        (p == placeholder).then_some(o.as_str())
    }

    /// `tail` 是不是某个占位符的**严格**前缀。
    ///
    /// 以 `tail` 开头的占位符在排好的序里连成一段，打头的就是第一个不小于 `tail` 的；
    /// 它要是正好等于 `tail`（不是严格前缀），这一段里还有的话就是紧跟着的那个。所以
    /// 看两个就够了。
    fn viable(&self, tail: &[u8]) -> bool {
        self.entries[self.seek(tail)..]
            .iter()
            .take(2)
            .any(|(p, _)| p.len() > tail.len() && p.as_bytes().starts_with(tail))
    }
}

impl Restorer {
    pub fn new(ledger: &Ledger) -> Self {
        let open = ledger.scheme().open;
        Self {
            book: Arc::new(Book::of(ledger)),
            lead: open.as_bytes()[0],
            open,
            buffer: String::new(),
        }
    }

    /// 同一本账、缓冲是空的另一个还原器。一条流里有好几路时各路一个，共用一份账
    /// （不各抄一份、各排一次序）。
    pub fn fresh(&self) -> Self {
        Self {
            book: Arc::clone(&self.book),
            lead: self.lead,
            open: self.open,
            buffer: String::new(),
        }
    }

    /// 没东西要还原。**调用方据此整条短路** —— 没脱敏的请求不该为这个
    /// 功能付任何延迟。
    pub fn is_noop(&self) -> bool {
        self.book.entries.is_empty()
    }

    /// 喂下一段，返回现在可以安全发出去的部分。
    pub fn process(&mut self, next: &str) -> String {
        if self.is_noop() {
            return next.to_string();
        }
        self.buffer.push_str(next);
        let cut = self.hold_from();
        if cut == 0 {
            return String::new();
        }
        let emit = self.buffer[..cut].to_string();
        self.buffer.drain(..cut);
        self.restore(&emit)
    }

    /// 流结束了，把扣住的那点吐出来。
    ///
    /// **残留的半截原样发出去。**一个到流末尾都没收尾的占位符开头永远不会
    /// 变成占位符了，那就让客户端看到上游真正说了什么。
    pub fn flush(&mut self) -> String {
        if self.buffer.is_empty() {
            return String::new();
        }
        let out = self.restore(&self.buffer.clone());
        self.buffer.clear();
        out
    }

    /// 不走缓冲的一次性还原（错误信息之类）。
    ///
    /// **不碰内部缓冲**：上游报错打断了一条正卡在占位符中间的流时，
    /// 后面那次 `flush()` 仍然要能正常收尾。
    pub fn oneshot(&self, s: &str) -> String {
        self.restore(s)
    }

    /// 从哪个字节开始必须扣住。返回 `buffer.len()` 表示全都能发。
    ///
    /// 切点总落在占位符开头那个 ASCII 字节上，所以一定是字符边界。
    fn hold_from(&self) -> usize {
        let buf = self.buffer.as_bytes();
        let window = buf.len().saturating_sub(self.book.longest);
        for p in window..buf.len() {
            if buf[p] == self.lead && self.book.viable(&buf[p..]) {
                return p;
            }
        }
        buf.len()
    }

    fn restore(&self, s: &str) -> String {
        if self.is_noop() {
            return s.to_string();
        }
        crate::redact::replace::swap_with(
            s,
            self.open,
            &self.book.lens,
            |ph| self.book.original(ph),
            str::to_string,
        )
    }
}

/// 字节流上的还原器。
///
/// [`Restorer`] 吃的是 `&str`，而响应是一串 `Bytes` —— **一个多字节字符
/// 可能被切在两个 chunk 中间**。这一层负责扣住那半个字符，等下一块到了
/// 再拼。
///
/// 这个项目已经被字节切片坑过三次，所以它是一个独立的类型，
/// 而不是调用方各自写一遍 `from_utf8_lossy`：那个函数会把半个字符变成
/// `�`，而那是**不可逆**的 —— 客户端拿到的文字里会多出一个替换符。
pub struct ByteRestorer {
    inner: Restorer,
    /// 上一块结尾那半个字符
    partial: Vec<u8>,
}

impl ByteRestorer {
    pub fn new(ledger: &Ledger) -> Self {
        Self {
            inner: Restorer::new(ledger),
            partial: Vec::new(),
        }
    }
    pub fn is_noop(&self) -> bool {
        self.inner.is_noop()
    }

    pub fn process(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.is_noop() {
            return chunk.to_vec();
        }
        let mut buf = std::mem::take(&mut self.partial);
        buf.extend_from_slice(chunk);
        let valid = match std::str::from_utf8(&buf) {
            Ok(_) => buf.len(),
            Err(e) => e.valid_up_to(),
        };
        // 尾巴上那半个字符留到下一块
        self.partial = buf[valid..].to_vec();
        let text = std::str::from_utf8(&buf[..valid])
            .expect("the bytes before valid_up_to are valid UTF-8");
        self.inner.process(text).into_bytes()
    }

    /// 流结束了。**残留的半个字符原样发出去** —— 上游就是那么说的，
    /// 我们没有立场替它补全或者抹掉。
    pub fn flush(&mut self) -> Vec<u8> {
        let mut out = self.inner.flush().into_bytes();
        out.extend_from_slice(&self.partial);
        self.partial.clear();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redact::replace::{Scheme, redact};
    use crate::redact::rules::RuleSet;

    const KEY: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn ledger() -> Ledger {
        redact(
            &format!("k={KEY}"),
            &RuleSet::only(&["anthropic-api-key"]),
            Ledger::new(Scheme::SECRET),
        )
        .ledger
    }

    /// 把一段内容按给定的切法喂进去，返回客户端最终看到的东西。
    fn feed(chunks: &[&str]) -> String {
        let l = ledger();
        let mut r = Restorer::new(&l);
        let mut out = String::new();
        for c in chunks {
            out.push_str(&r.process(c));
        }
        out.push_str(&r.flush());
        out
    }

    #[test]
    fn a_placeholder_split_across_chunks_still_comes_back_whole() {
        // 这是这个文件存在的全部理由。
        assert_eq!(
            feed(&["你的 key 是 <<TW_SE", "CRET_1>> 对吗"]),
            format!("你的 key 是 {KEY} 对吗")
        );
        assert_eq!(feed(&["<<", "TW_SECRET_1", ">>"]), KEY);
        // 一个字节一个字节地喂，最坏的切法
        let text = "前 <<TW_SECRET_1>> 后";
        let one_by_one: Vec<String> = text.chars().map(|c| c.to_string()).collect();
        let refs: Vec<&str> = one_by_one.iter().map(|s| s.as_str()).collect();
        assert_eq!(feed(&refs), format!("前 {KEY} 后"));
    }

    #[test]
    fn cpp_stream_operators_do_not_stall_the_stream() {
        // **「最右边的 `<<` 之后没有 `>>` 就全扣住」在这里当场坏掉**：下面
        // 这段会一直卡到流结束。而「响应里有 `<<`」不是边角情况，是常态。
        let l = ledger();
        let mut r = Restorer::new(&l);
        let out = r.process("std::cout << x << std::endl;");
        assert_eq!(out, "std::cout << x << std::endl;", "被扣住了：{out:?}");
        assert_eq!(r.flush(), "");
    }

    #[test]
    fn a_heredoc_does_not_stall_either() {
        let l = ledger();
        let mut r = Restorer::new(&l);
        assert_eq!(
            r.process("cat <<EOF\n内容\nEOF\n"),
            "cat <<EOF\n内容\nEOF\n"
        );
    }

    #[test]
    fn only_a_viable_prefix_is_held_back() {
        let l = ledger();
        let mut r = Restorer::new(&l);
        // `<<TW_SEC` 有可能长成占位符 —— 扣住
        assert_eq!(r.process("abc <<TW_SEC"), "abc ");
        // 下一段证明它不是 —— 连本带利放出来
        assert_eq!(
            r.process("RETARY 的意思是秘书"),
            "<<TW_SECRETARY 的意思是秘书"
        );
    }

    #[test]
    fn the_output_equals_a_one_shot_restore_no_matter_how_it_is_cut() {
        // **拼起来必须和一次性还原完全一样。**顺序和内容都不能变，
        // 唯一的差别只是有些字节晚几毫秒发出去。
        let text = "开头 <<TW_SECRET_1>> 中间 a << b 结尾 <<TW_SECRET_1>>";
        let l = ledger();
        let want = crate::redact::replace::restore(text, &l);
        for size in 1..=text.len().min(40) {
            let mut chunks = Vec::new();
            let mut rest = text;
            while !rest.is_empty() {
                let mut n = size.min(rest.len());
                while !rest.is_char_boundary(n) {
                    n += 1;
                }
                chunks.push(&rest[..n]);
                rest = &rest[n..];
            }
            assert_eq!(feed(&chunks), want, "按 {size} 字节切的时候错了");
        }
    }

    #[test]
    fn an_unterminated_placeholder_at_the_end_is_emitted_verbatim() {
        // 到流末尾都没收尾的 `<<TW_SECRET_` 永远不会变成占位符了，
        // 那就让客户端看到上游真正说了什么。
        assert_eq!(feed(&["结果是 <<TW_SECRET_"]), "结果是 <<TW_SECRET_");
        assert_eq!(feed(&["半个 <"]), "半个 <");
    }

    #[test]
    fn nothing_is_ever_held_for_long() {
        // 扣住的永远不比最长的那个占位符长 —— 而不是整条响应。
        let l = ledger();
        let mut r = Restorer::new(&l);
        let long = format!("<<TW_SECRET_{}", "1".repeat(500));
        let out = r.process(&long);
        assert!(
            out.len() >= long.len() - "<<TW_SECRET_1>>".len(),
            "扣住了 {} 个字节",
            long.len() - out.len()
        );
    }

    #[test]
    fn a_placeholder_nobody_issued_is_not_waited_for() {
        // 模型自己编的 `<<TW_SECRET_9>>` 不在账本里，没有还原的可能，也就
        // 没有等的理由
        let l = ledger();
        let mut r = Restorer::new(&l);
        assert_eq!(r.process("看 <<TW_SECRET_9"), "看 <<TW_SECRET_9");
    }

    #[test]
    fn brace_placeholders_are_held_the_same_way_and_templates_are_not() {
        // 企业版的 `{{EMAIL_1}}`。模板里的 `{{ name }}` 不该被扣住
        let scheme = Scheme {
            open: "{{",
            close: "}}",
            label: "PII",
        };
        let rules = RuleSet::none()
            .with_labeled("email", r"[a-z]+@[a-z]+\.com", Some("EMAIL"))
            .unwrap();
        let t = "找 a@b.com";
        let hits = crate::redact::rules::scan_plain(t, &rules);
        let l = crate::redact::replace::apply(t, &hits, Ledger::new(scheme)).ledger;
        let mut r = Restorer::new(&l);
        assert_eq!(r.process("Hello {{ name }}, {{EMA"), "Hello {{ name }}, ");
        assert_eq!(r.process("IL_1}} 好"), "a@b.com 好");
    }

    #[test]
    fn a_stream_with_nothing_to_restore_never_buffers() {
        // 没脱敏的请求不该为这个功能付任何延迟。
        let mut r = Restorer::new(&Ledger::new(Scheme::SECRET));
        assert!(r.is_noop());
        assert_eq!(
            r.process("<<TW_SECRET_1>> 原样过去"),
            "<<TW_SECRET_1>> 原样过去"
        );
        assert_eq!(r.flush(), "");
    }

    #[test]
    fn a_oneshot_restore_does_not_disturb_the_buffered_tail() {
        // 上游报错打断了一条正卡在占位符中间的流 —— 后面那次 flush()
        // 仍然要能正常收尾。
        let l = ledger();
        let mut r = Restorer::new(&l);
        assert_eq!(r.process("前 <<TW_SECRET_"), "前 ");
        assert_eq!(
            r.oneshot("错误里也有 <<TW_SECRET_1>>"),
            format!("错误里也有 {KEY}")
        );
        assert_eq!(r.process("1>> 后"), format!("{KEY} 后"));
    }

    /// 二分查排好序的账，和拿账里每个占位符挨个比，扣住的位置一模一样；切成多少段喂，
    /// 拼起来都和一次性还原一样
    #[test]
    fn holding_back_agrees_with_checking_every_placeholder() {
        use crate::redact::rules::{Hit, Rule};
        // 一本几种标签、几种号长都有的账
        let mut text = String::new();
        let mut hits = Vec::new();
        for i in 0..150 {
            let value = format!("value-{i}");
            hits.push(Hit {
                bytes: text.len()..text.len() + value.len(),
                rule: Rule::Builtin("aws-access-key-id"),
                label: [None, Some("TW_ID_NUMBER"), Some("TW_EMAIL")][i % 3].map(Arc::from),
            });
            text.push_str(&value);
            text.push(' ');
        }
        let l = crate::redact::replace::apply(&text, &hits, Ledger::new(Scheme::SECRET)).ledger;
        let table = l.table();
        let longest = table.keys().map(String::len).max().unwrap();
        let every = |buf: &str| {
            let buf = buf.as_bytes();
            for p in buf.len().saturating_sub(longest)..buf.len() {
                if buf[p] == b'<'
                    && table
                        .keys()
                        .any(|k| k.len() > buf.len() - p && k.as_bytes().starts_with(&buf[p..]))
                {
                    return p;
                }
            }
            buf.len()
        };
        let keys: Vec<&String> = table.keys().collect();
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let mut r = Restorer::new(&l);
        for _ in 0..2000 {
            // 一段话，尾巴上是某个占位符（或者别的什么）的一截
            let k = keys[next(keys.len())];
            let mut buf = ["", "前面 ", "a << b ", "中文"][next(4)].to_string();
            match next(4) {
                0 => buf.push_str(&k[..1 + next(k.len())]),
                1 => buf.push_str(&k[..next(k.len())]),
                2 => {
                    buf.push_str(["<<TW_SECRET_9999", "<<TW_x", "<<<", "<", "<<TW_EMAIL_"][next(5)])
                }
                _ => buf.push_str(&format!("{k}{}", &k[..next(k.len())])),
            }
            r.buffer = buf.clone();
            assert_eq!(r.hold_from(), every(&buf), "{buf:?}");
        }
        // 整段切碎了喂，拼起来和一次性还原一样
        let mut whole = String::new();
        for _ in 0..300 {
            whole.push_str(keys[next(keys.len())]);
            whole.push_str(["", " ", "<<", "中", "<<TW_SECRET_"][next(5)]);
        }
        let want = crate::redact::replace::restore(&whole, &l);
        for size in [1, 2, 3, 7, 16, 64] {
            let mut r = Restorer::new(&l);
            let mut out = String::new();
            let mut at = 0;
            while at < whole.len() {
                let mut end = (at + size).min(whole.len());
                while !whole.is_char_boundary(end) {
                    end += 1;
                }
                out.push_str(&r.process(&whole[at..end]));
                at = end;
            }
            out.push_str(&r.flush());
            assert_eq!(out, want, "按 {size} 字节切的时候");
        }
    }

    #[test]
    fn multibyte_content_is_never_cut_in_half() {
        // buffer 是 String，切点必须落在字符边界上 —— 这个项目已经被
        // 字节切片坑过三次。
        let text = "中文中文中文 <<TW_SECRET_1>> 中文中文中文";
        for size in 1..20 {
            let mut chunks = Vec::new();
            let mut rest = text;
            while !rest.is_empty() {
                let mut n = size.min(rest.len());
                while !rest.is_char_boundary(n) {
                    n += 1;
                }
                chunks.push(&rest[..n]);
                rest = &rest[n..];
            }
            assert!(feed(&chunks).contains(KEY), "按 {size} 切的时候没还原");
        }
    }
}

#[cfg(test)]
mod byte_tests {
    use super::*;
    use crate::redact::replace::{Scheme, redact};
    use crate::redact::rules::RuleSet;

    const KEY: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn ledger() -> Ledger {
        redact(
            &format!("k={KEY}"),
            &RuleSet::only(&["anthropic-api-key"]),
            Ledger::new(Scheme::SECRET),
        )
        .ledger
    }

    fn feed_bytes(chunks: &[&[u8]]) -> String {
        let l = ledger();
        let mut r = ByteRestorer::new(&l);
        let mut out = Vec::new();
        for c in chunks {
            out.extend_from_slice(&r.process(c));
        }
        out.extend_from_slice(&r.flush());
        String::from_utf8(out).expect("输出必须还是合法 UTF-8")
    }

    #[test]
    fn a_multibyte_character_split_across_chunks_is_not_mangled() {
        // `from_utf8_lossy` 会把半个字符变成 `�`，而那是**不可逆**的
        // —— 客户端拿到的文字里会永远多出一个替换符。
        let text = "中文中文 <<TW_SECRET_1>> 中文中文";
        let bytes = text.as_bytes();
        for cut in 1..bytes.len() {
            let got = feed_bytes(&[&bytes[..cut], &bytes[cut..]]);
            assert!(
                !got.contains('\u{fffd}'),
                "在第 {cut} 字节切开时出现了替换符：{got}"
            );
            assert!(got.contains(KEY), "在第 {cut} 字节切开时没还原");
        }
    }

    #[test]
    fn a_byte_at_a_time_still_produces_the_right_text() {
        let text = "前 <<TW_SECRET_1>> 后";
        let chunks: Vec<&[u8]> = text.as_bytes().chunks(1).collect();
        assert_eq!(feed_bytes(&chunks), format!("前 {KEY} 后"));
    }

    #[test]
    fn a_truncated_character_at_the_end_of_a_stream_is_passed_through() {
        // 上游就是那么说的，我们没有立场替它补全或者抹掉。
        let l = ledger();
        let mut r = ByteRestorer::new(&l);
        let half = &"中".as_bytes()[..2];
        assert_eq!(r.process(half), Vec::<u8>::new());
        assert_eq!(r.flush(), half.to_vec());
    }

    #[test]
    fn a_stream_with_nothing_to_restore_is_copied_straight_through() {
        let mut r = ByteRestorer::new(&Ledger::new(Scheme::SECRET));
        assert!(r.is_noop());
        let raw = &[0xff, 0xfe, 0x00][..];
        assert_eq!(r.process(raw), raw.to_vec(), "二进制体也该原样过去");
    }
}
