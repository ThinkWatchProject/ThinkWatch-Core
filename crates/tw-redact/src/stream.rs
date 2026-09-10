//! 流式还原：占位符被切在两个 chunk 中间时怎么办（DESIGN.md §5.1）。
//!
//! 做法和企业版的 `PiiStreamRestorer` 一样：**扣住尾部那段「还可能长成
//! 占位符」的内容不发**，等下一个 chunk 到了再判断。
//!
//! # 一处必须改掉的地方
//!
//! 企业版的判据是「最右边那个 `{{` 之后没有 `}}` 就全扣住」。照搬到
//! `<<TW_SECRET_n>>` 上会**当场坏掉**：
//!
//! ```text
//! std::cout << x << std::endl;    ← C++ 满屏都是
//! cat <<EOF                       ← heredoc 同样常见
//! ```
//!
//! 按那个判据，`<<` 之后的所有内容会一直扣着，直到某处出现 `>>` 或者流
//! 结束 —— 用户看到的是**响应卡住**。而这是个给写代码的人用的网关，
//! 「响应里有 `<<`」不是边角情况，是常态。
//!
//! 所以判据换成：**扣住的那一段必须是占位符的一个合法前缀**。
//! `<< x` 里 `<<` 后面是空格，一眼就不可能长成 `<<TW_SECRET_`，立刻放行。
//! 顺带还加了个长度上限 —— 就算判据写漏了，扣住的也永远是几十个字节。

use crate::redact::{CLOSE, Ledger, OPEN};

/// 扣住的字节数上限。`<<TW_SECRET_` + 一串数字 + `>>`，几十字节封顶。
const MAX_HOLD: usize = 48;

/// `tail` 有没有可能是某个占位符的开头。
///
/// 文法就是 `<<TW_SECRET_` + 至少一位数字 + `>>`。
fn viable_prefix(tail: &[u8]) -> bool {
    if tail.len() > MAX_HOLD {
        return false;
    }
    let open = OPEN.as_bytes();
    let close = CLOSE.as_bytes();
    // 还在 `<<TW_SECRET_` 里面
    if tail.len() <= open.len() {
        return tail == &open[..tail.len()];
    }
    if &tail[..open.len()] != open {
        return false;
    }
    let rest = &tail[open.len()..];
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == rest.len() {
        // 数字还在往下写
        return true;
    }
    // 数字写完了，接下来只能是 `>` 或者 `>>`
    let after = &rest[digits..];
    digits > 0 && after.len() < close.len() && after == &close[..after.len()]
}

/// 从哪个字节开始必须扣住。返回 `buf.len()` 表示全都能发。
fn hold_from(buf: &[u8]) -> usize {
    let window = buf.len().saturating_sub(MAX_HOLD);
    for p in window..buf.len() {
        if buf[p] == b'<' && viable_prefix(&buf[p..]) {
            return p;
        }
    }
    buf.len()
}

/// 一条流上的还原器。
///
/// **`process()` 的输出拼起来再接上 `flush()`，等于把整段内容一次性
/// 还原的结果。**顺序和内容都不变，唯一的差别是有些字节晚几毫秒发出去。
pub struct Restorer {
    table: std::collections::HashMap<String, String>,
    buffer: String,
}

impl Restorer {
    pub fn new(ledger: &Ledger) -> Self {
        Self {
            table: ledger.table().clone(),
            buffer: String::new(),
        }
    }

    /// 没东西要还原。**调用方据此整条短路** —— 没脱敏的请求不该为这个
    /// 功能付任何延迟。
    pub fn is_noop(&self) -> bool {
        self.table.is_empty()
    }

    /// 喂下一段，返回现在可以安全发出去的部分。
    pub fn process(&mut self, next: &str) -> String {
        if self.is_noop() {
            return next.to_string();
        }
        self.buffer.push_str(next);
        let cut = hold_from(self.buffer.as_bytes());
        if cut == 0 {
            return String::new();
        }
        let emit = self.buffer[..cut].to_string();
        self.buffer.drain(..cut);
        self.restore(&emit)
    }

    /// 流结束了，把扣住的那点吐出来。
    ///
    /// **残留的半截原样发出去。**一个到流末尾都没收尾的 `<<TW_SECRET_`
    /// 永远不会变成占位符了，那就让客户端看到上游真正说了什么。
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

    fn restore(&self, s: &str) -> String {
        if !s.contains(OPEN) {
            return s.to_string();
        }
        let mut out = s.to_string();
        for (ph, original) in &self.table {
            if out.contains(ph.as_str()) {
                out = out.replace(ph.as_str(), original);
            }
        }
        out
    }
}

/// 字节流上的还原器。
///
/// [`Restorer`] 吃的是 `&str`，而响应是一串 `Bytes` —— **一个多字节字符
/// 可能被切在两个 chunk 中间**。这一层负责扣住那半个字符，等下一块到了
/// 再拼。
///
/// 这个项目已经被字节切片坑过三次（§9.7），所以它是一个独立的类型，
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
        let text = std::str::from_utf8(&buf[..valid]).expect("valid_up_to 保证这一段是合法的");
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
    use crate::redact::redact;
    use crate::rules::Kind;

    const KEY: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn ledger() -> Ledger {
        redact(&format!("k={KEY}"), &[Kind::ApiKeys]).ledger
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
        // **企业版的判据在这里当场坏掉。**按「最右边的 `<<` 之后没有
        // `>>` 就全扣住」，下面这段会一直卡到流结束。而这是个给写代码的
        // 人用的网关 —— 「响应里有 `<<`」不是边角情况，是常态。
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
        let want = crate::redact::restore(text, &l);
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
        // 就算判据写漏了，扣住的也永远是几十个字节 —— 而不是整条响应。
        let l = ledger();
        let mut r = Restorer::new(&l);
        let long = format!("<<TW_SECRET_{}", "1".repeat(500));
        let out = r.process(&long);
        assert!(
            out.len() >= long.len() - MAX_HOLD,
            "扣住了 {} 个字节",
            long.len() - out.len()
        );
    }

    #[test]
    fn a_stream_with_nothing_to_restore_never_buffers() {
        // 没脱敏的请求不该为这个功能付任何延迟。
        let mut r = Restorer::new(&Ledger::default());
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

    #[test]
    fn multibyte_content_is_never_cut_in_half() {
        // buffer 是 String，切点必须落在字符边界上 —— 这个项目已经被
        // 字节切片坑过三次（§9.7）。
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
    use crate::redact::redact;
    use crate::rules::Kind;

    const KEY: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn ledger() -> Ledger {
        redact(&format!("k={KEY}"), &[Kind::ApiKeys]).ledger
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
        let mut r = ByteRestorer::new(&Ledger::default());
        assert!(r.is_noop());
        let raw = &[0xff, 0xfe, 0x00][..];
        assert_eq!(r.process(raw), raw.to_vec(), "二进制体也该原样过去");
    }
}
