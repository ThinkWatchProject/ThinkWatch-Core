//! 流式插件在一块文字里改没改：交给它的全部文字拼起来，和它交出的全部文字拼起来（都是
//! 占位符的写法）比。
//!
//! 逐段插件扣字是常事（等一个词收全、等正则看清后文），只扣着、到块结束原样放出的不算
//! 改了文字。比的时候**不把整块存两份**：两边对上的部分随到随丢，只留还没对上的那一截；
//! 一处对不上就记下「改了」，之后不再比。

/// 一块文字的账。
#[derive(Debug, Default)]
pub(super) struct Tally {
    /// 还没对上的那一截：`ahead` 为真时是插件多交出的，否则是交给它、它还没交回的
    gap: String,
    ahead: bool,
    differs: bool,
}

impl Tally {
    /// 交给插件一段、它交回一段（没改就是同一段）
    pub(super) fn pass(&mut self, given: &str, returned: &str) {
        // 最常见的：前面都对上了，这一段原样交回
        if self.gap.is_empty() && !self.differs && given == returned {
            return;
        }
        self.push(given, false);
        self.push(returned, true);
    }

    /// 只有交回的（块结束时 `onReplyTextEnd` 补的）
    pub(super) fn returned(&mut self, s: &str) {
        self.push(s, true);
    }

    fn push(&mut self, s: &str, out: bool) {
        if self.differs || s.is_empty() {
            return;
        }
        if self.gap.is_empty() || self.ahead == out {
            self.gap.push_str(s);
            self.ahead = out;
        } else if self.gap.starts_with(s) {
            // 另一边先到的那一截，这一段对上了它的开头
            self.gap.drain(..s.len());
        } else if s.starts_with(self.gap.as_str()) {
            // 这一段把先到的那一截整个对上了，多出来的轮到这一边先到
            self.gap = s[self.gap.len()..].to_string();
            self.ahead = out;
        } else {
            self.differs = true;
            self.gap = String::new();
        }
    }

    /// 块结束了：两边拼起来不一样
    pub(super) fn changed(&self) -> bool {
        self.differs || !self.gap.is_empty()
    }

    /// 插件半路出错被拿掉了：它还没交回的那些原样往下走，不算它改的；它交出的和交给它的
    /// 对不上，或者多交出了没收到过的字，算
    pub(super) fn changed_so_far(&self) -> bool {
        self.differs || (self.ahead && !self.gap.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(steps: &[(&str, &str)], end: &str) -> Tally {
        let mut t = Tally::default();
        for (given, returned) in steps {
            t.pass(given, returned);
        }
        t.returned(end);
        t
    }

    #[test]
    fn holding_back_and_releasing_the_same_text_is_not_a_change() {
        // 原样交回
        assert!(!run(&[("ab", "ab"), ("cd", "cd")], "").changed());
        // 扣到最后
        assert!(!run(&[("ab", ""), ("cd", "")], "abcd").changed());
        // 扣一截、补一截，切在别处
        assert!(!run(&[("abc", "a"), ("def", "bcde")], "f").changed());
        // 交出的比收到的切得碎
        assert!(!run(&[("abcdef", "ab"), ("", "cd"), ("g", "efg")], "").changed());
    }

    #[test]
    fn any_difference_in_the_joined_text_is_a_change() {
        assert!(run(&[("ab", "aB")], "").changed());
        assert!(run(&[("ab", ""), ("cd", "")], "ABCD").changed());
        // 少交了
        assert!(run(&[("ab", "a")], "").changed());
        // 多交了
        assert!(run(&[("ab", "ab")], "!").changed());
        // 顺序换了
        assert!(run(&[("ab", ""), ("cd", "cd")], "ab").changed());
        // 一处不同之后再对上也算改了
        assert!(run(&[("ab", "xb"), ("cd", "cd")], "").changed());
    }

    #[test]
    fn text_handed_out_before_it_arrived_counts_once_it_arrives() {
        // 插件先交出了还没收到的字（猜中了）：拼起来一样就不算
        let mut t = Tally::default();
        t.pass("ab", "abc");
        assert!(t.changed_so_far());
        t.pass("cd", "d");
        assert!(!t.changed());
    }

    #[test]
    fn a_plugin_dropped_halfway_counts_only_what_it_changed() {
        // 扣着的原样往下走：不算它改的
        let mut t = Tally::default();
        t.pass("abc", "a");
        assert!(!t.changed_so_far());
        // 改过的：算
        t.pass("d", "X");
        assert!(t.changed_so_far());
    }
}
