//! 插件看不到真的密钥（约定 I5）。
//!
//! 进插件之前，按**出站脱敏的规则**把认得出的密钥换成占位符（`<<TW_SECRET_1>>`）；
//! 插件交回来之后再把占位符换回去。**不看脱敏开在哪一档**：观察档、关闭时请求原样
//! 发给上游，但插件看到的照样是占位符 —— 档位管的是上游看到什么，这里管的是插件。
//!
//! 一个请求一本账（[`Bridge`]）：**和出站脱敏同一套编号** —— 客户端原文里认得出的值按
//! 出现的先后编号，让开原文里本来就写着的占位符（[`crate::guard::look`] 在拦截档下就是
//! 这么编的，插件跑过的请求它接着这本账编，见 [`crate::guard::look_from`]）。同一个值
//! 在插件那儿、在每一跳、在存下来的请求和回答里都是同一个占位符。账里没有的（模型
//! 自己写出来的一把 key）在换的时候按规则再找一遍，接着编号记进账里。
//!
//! 只认**规则认得出的**：规则全关掉的话没有什么可换的，那是用户自己的选择。
//!
//! **占位符只管脱敏，换回去不看是谁写的。**插件交回来的东西里的占位符一律按这本账换回
//! 原值，和脱敏对上游的回答做的一样 —— 插件写下一个它没见过的占位符，客户端拿到的就是
//! 那个原值。危险的工具调用归工具调用审查管：它看的是换回之后、客户端要执行的那一个调用，
//! 把凭据发往陌生主机的，内置规则 `secret-to-unknown-host` 在拦截档下切断。

use std::collections::{BTreeMap, HashMap};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::Arc;

use serde_json::Value;
use tw_guard::redact::replace::{Ledger, Scheme};
use tw_guard::redact::rules::RuleSet;

/// 一个请求的密钥映射。
#[derive(Clone)]
pub struct Bridge {
    rules: Arc<RuleSet>,
    ledger: Ledger,
    /// 账里的值和它的占位符，长的在前、一样长的按字典序：换的时候长的先换，一个值是另一个
    /// 的一部分时不会只换半截（见 [`Bridge::hide`]）
    values: Vec<(String, String)>,
    /// 一段文字里账里的值都在哪儿（见 [`Lengths`]）
    lengths: Lengths,
    /// `values` 的下标，按值的字节序排：一段尾巴是不是哪个值的开头，二分查一次（见
    /// [`Bridge::hold_from`]）
    by_bytes: Vec<usize>,
}

impl Bridge {
    pub fn new(rules: Arc<RuleSet>) -> Self {
        Self {
            rules,
            ledger: Ledger::new(Scheme::SECRET),
            values: Vec::new(),
            lengths: Lengths::default(),
            by_bytes: Vec::new(),
        }
    }

    /// 账是空的：什么都不用换
    pub fn is_empty(&self) -> bool {
        self.ledger.is_empty()
    }

    /// 按客户端发来的那份 JSON 请求体编号：认得出的值按出现的先后发号，让开原文里本来
    /// 就写着的占位符。**和拦截档下出站脱敏编的是同一套号**（同一个找法、同一个起点）。
    pub fn learn(&mut self, body: &[u8]) {
        let Ok(text) = std::str::from_utf8(body) else {
            return;
        };
        let seed = std::mem::replace(&mut self.ledger, Ledger::new(Scheme::SECRET)).avoiding(text);
        let hits = if self.rules.is_empty() {
            Vec::new()
        } else {
            crate::guard::hits(text, &self.rules)
        };
        self.ledger = if hits.is_empty() {
            seed
        } else {
            tw_guard::redact::replace::apply(text, &hits, seed).ledger
        };
        self.reindex();
    }

    /// 这本账。出站脱敏接着它编号
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// 换成另一本账（拦截档下成功那一跳的：它接着这个请求的账编，回答里的占位符按它）
    pub fn with_ledger(mut self, ledger: Ledger) -> Self {
        self.ledger = ledger;
        self.reindex();
        self
    }

    fn reindex(&mut self) {
        let mut values: Vec<(String, String)> = self
            .ledger
            .replacements()
            .filter(|(o, _)| !o.is_empty())
            .map(|(o, p)| (o.to_string(), p.to_string()))
            .collect();
        values.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        let mut by_bytes: Vec<usize> = (0..values.len()).collect();
        by_bytes.sort_unstable_by(|&a, &b| values[a].0.cmp(&values[b].0));
        self.lengths = Lengths::of(&values);
        self.by_bytes = by_bytes;
        self.values = values;
    }

    /// 一段文字里的密钥换成占位符：账里的值，加上按规则新找到的。
    pub fn hide(&mut self, s: &str) -> String {
        let cur = self.hide_known(s);
        if self.rules.is_empty() {
            return cur;
        }
        let mut hits = tw_guard::redact::rules::scan_text(&cur, &self.rules);
        // 压在一个占位符上的不算（连接串规则会把 `app:<<TW_SECRET_2>>@` 当成口令）
        tw_guard::redact::flow::off_placeholders(&cur, &mut hits);
        if hits.is_empty() {
            return cur;
        }
        let ledger = std::mem::replace(&mut self.ledger, Ledger::new(Scheme::SECRET));
        let r = tw_guard::redact::replace::apply(&cur, &hits, ledger);
        self.ledger = r.ledger;
        self.reindex();
        r.text
    }

    /// 账里的值换成占位符。
    ///
    /// **换出来的和「长的先换、一样长的按字典序，一个值一个值地整段 `replace`」一样**，只是
    /// 不拿每个值去整段里各找一遍 —— 账里几万个值时，每段文字都要找几万遍。先一遍找出
    /// 每个值出现的每一处（互相重叠的也算），再照那个先后挑：一处被先换的值占了的地方，
    /// 后换的值在那儿就不在了；同一个值的几处互相重叠时，`replace` 从左往右取不重叠的，
    /// 这里也是。
    ///
    /// 和一个个 `replace` 只差在一种情况上：后换的值碰巧是**刚换上去的那个占位符**里的一截
    /// （一个口令就是 `1`），一个个换会把占位符换坏，这里不会 —— 找的是原文。
    fn hide_known(&self, s: &str) -> String {
        let mut found = self.lengths.find(&self.values, s);
        if found.is_empty() {
            return s.to_string();
        }
        // 下标就是先后（`values` 照换的先后排好了），同一个值从左往右
        found.sort_unstable();
        // 挑中的：起点 → (终点, 哪个值)。互不重叠，所以只看起点在它前面的最后一处
        let mut taken: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
        for (v, start) in found {
            let end = start + self.values[v].0.len();
            let clash = taken
                .range(..end)
                .next_back()
                .is_some_and(|(_, &(e, _))| e > start);
            if !clash {
                taken.insert(start, (end, v));
            }
        }
        let mut out = String::with_capacity(s.len());
        let mut at = 0;
        for (start, (end, v)) in taken {
            out.push_str(&s[at..start]);
            out.push_str(&self.values[v].1);
            at = end;
        }
        out.push_str(&s[at..]);
        out
    }

    /// 占位符换回原值
    pub fn reveal(&self, s: &str) -> String {
        tw_guard::redact::replace::restore(s, &self.ledger)
    }

    /// 一个 JSON 值里的每个字符串（连同对象的键）都换成占位符。
    pub fn hide_value(&mut self, v: &mut Value) {
        match v {
            Value::String(s) => {
                let next = self.hide(s);
                if next != *s {
                    *s = next;
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|i| self.hide_value(i)),
            Value::Object(m) => {
                let keys: Vec<String> = m.keys().cloned().collect();
                for k in keys {
                    let hidden = self.hide(&k);
                    if hidden != k
                        && let Some(mut x) = m.remove(&k)
                    {
                        self.hide_value(&mut x);
                        m.insert(hidden, x);
                    } else if let Some(x) = m.get_mut(&k) {
                        self.hide_value(x);
                    }
                }
            }
            _ => {}
        }
    }

    /// [`Bridge::hide_value`] 反过来
    pub fn reveal_value(&self, v: &mut Value) {
        if self.is_empty() {
            return;
        }
        match v {
            Value::String(s) => {
                let next = self.reveal(s);
                if next != *s {
                    *s = next;
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|i| self.reveal_value(i)),
            Value::Object(m) => {
                let keys: Vec<String> = m.keys().cloned().collect();
                for k in keys {
                    let shown = self.reveal(&k);
                    if shown != k
                        && let Some(mut x) = m.remove(&k)
                    {
                        self.reveal_value(&mut x);
                        m.insert(shown, x);
                    } else if let Some(x) = m.get_mut(&k) {
                        self.reveal_value(x);
                    }
                }
            }
            _ => {}
        }
    }

    /// 流式给插件文字时，`buf` 从哪个字节起要先扣住：尾巴是账里某个值的开头，下一段
    /// 可能把它补全 —— 半截的值送进去，插件就看到了真值的一部分，换也换不掉。
    ///
    /// 返回 `buf.len()` 是全都能给。切点总在字符边界上。
    ///
    /// 从长到短试 `buf` 的每一截尾巴，是哪个值的真前缀就从那儿扣：按字节序排好的值里
    /// 二分查一次，不拿每个值的每个前缀去比 —— 后者在流式的每一段上都要比「账里有几个
    /// 值 × 值有多长」次。
    pub fn hold_from(&self, buf: &str) -> usize {
        let longest = self.values.first().map_or(0, |(o, _)| o.len());
        for k in (1..longest.min(buf.len() + 1)).rev() {
            let at = buf.len() - k;
            if buf.is_char_boundary(at) && self.starts_a_value(&buf.as_bytes()[at..]) {
                return at;
            }
        }
        buf.len()
    }

    /// `tail` 是不是账里某个值的**真**前缀。以它开头的值在字节序里连成一段，打头的是第一
    /// 个不小于它的；那个正好等于它的话（不是真前缀），这一段里还有的就是紧跟着的那个
    fn starts_a_value(&self, tail: &[u8]) -> bool {
        let first = self
            .by_bytes
            .partition_point(|&v| self.values[v].0.as_bytes() < tail);
        self.by_bytes[first..].iter().take(2).any(|&v| {
            let o = self.values[v].0.as_bytes();
            o.len() > tail.len() && o.starts_with(tail)
        })
    }
}

/// 账里的值按长度分组，在一段文字里一遍找出它们出现的每一处（见 [`Bridge::hide_known`]）。
///
/// 每一种长度在文字上滚一遍哈希（Rabin–Karp），哈希对上了再逐字节核对：一段文字的
/// 代价是「文字长度 × 值有几种长度」，和账里有几个值无关 —— 同一种密钥一样长，几万把
/// AWS 访问密钥只是一种长度。哈希撞了只多核对一次，不会换错。
#[derive(Clone, Default)]
struct Lengths {
    groups: Vec<Group>,
}

/// 一样长的那些值
#[derive(Clone)]
struct Group {
    len: usize,
    /// `BASE` 的 `len - 1` 次方：滚动时减掉移出窗口的那个字节用
    top: u64,
    /// 哈希 → 这么长的值（`values` 的下标）
    by_hash: HashMap<u64, Vec<usize>, BuildHasherDefault<Spread>>,
}

/// 滚动哈希的底
const BASE: u64 = 0x0100_0000_01b3;

/// 一段字节的滚动哈希。字节加一，免得开头的 `\0` 不算数
fn rolling(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0, |h: u64, &c| {
        h.wrapping_mul(BASE).wrapping_add(u64::from(c) + 1)
    })
}

impl Lengths {
    /// `values` 已经按长度从长到短排好：一样长的连在一起
    fn of(values: &[(String, String)]) -> Self {
        let mut groups: Vec<Group> = Vec::new();
        for (v, (original, _)) in values.iter().enumerate() {
            let len = original.len();
            if groups.last().is_none_or(|g| g.len != len) {
                groups.push(Group {
                    len,
                    top: (1..len).fold(1, |p: u64, _| p.wrapping_mul(BASE)),
                    by_hash: HashMap::default(),
                });
            }
            if let Some(g) = groups.last_mut() {
                g.by_hash
                    .entry(rolling(original.as_bytes()))
                    .or_default()
                    .push(v);
            }
        }
        Self { groups }
    }

    /// `s` 里每一处账里的值：(`values` 的下标, 起点)。互相重叠的都在
    fn find(&self, values: &[(String, String)], s: &str) -> Vec<(usize, usize)> {
        let b = s.as_bytes();
        let mut found = Vec::new();
        for g in self.groups.iter().filter(|g| g.len <= b.len()) {
            let mut h = rolling(&b[..g.len]);
            for at in 0..=b.len() - g.len {
                if at > 0 {
                    let (out, inn) = (b[at - 1], b[at + g.len - 1]);
                    h = h
                        .wrapping_sub((u64::from(out) + 1).wrapping_mul(g.top))
                        .wrapping_mul(BASE)
                        .wrapping_add(u64::from(inn) + 1);
                }
                // 值是完整的 UTF-8，字节对得上的地方自然落在字符边界上
                for &v in g.by_hash.get(&h).into_iter().flatten() {
                    if values[v].0.as_bytes() == &b[at..at + g.len] {
                        found.push((v, at));
                    }
                }
            }
        }
        found
    }
}

/// 滚动哈希的低位分布得不匀（底是奇数，最低一位只看字节和的奇偶），进哈希表之前再
/// 搅一下（splitmix64 的收尾）
#[derive(Default)]
struct Spread(u64);

impl Hasher for Spread {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(b);
        }
    }
    fn write_u64(&mut self, x: u64) {
        self.0 = x;
    }
    fn finish(&self) -> u64 {
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";
    const OTHER: &str = "ghp_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

    fn rules() -> Arc<RuleSet> {
        Arc::new(RuleSet::defaults())
    }

    #[test]
    fn a_key_in_the_body_is_hidden_wherever_it_shows_up_and_comes_back() {
        let mut b = Bridge::new(rules());
        b.learn(
            serde_json::json!({"messages": [{"content": format!("k={KEY}")}]})
                .to_string()
                .as_bytes(),
        );
        let shown = b.hide(&format!("用这把 {KEY} 试试"));
        assert!(!shown.contains(KEY), "{shown}");
        assert!(shown.contains("<<TW_SECRET_1>>"), "{shown}");
        assert_eq!(b.reveal(&shown), format!("用这把 {KEY} 试试"));
    }

    #[test]
    fn a_key_the_body_did_not_have_is_found_and_numbered_after_the_known_ones() {
        let mut b = Bridge::new(rules());
        b.learn(format!("{{\"a\":\"{KEY}\"}}").as_bytes());
        let shown = b.hide(&format!("新的 {OTHER}"));
        assert_eq!(shown, "新的 <<TW_SECRET_2>>");
        assert_eq!(b.reveal(&shown), format!("新的 {OTHER}"));
    }

    #[test]
    fn values_inside_json_including_keys_are_hidden_and_restored() {
        let mut b = Bridge::new(rules());
        let mut v = serde_json::json!({"cmd": format!("export K={KEY}"), KEY: [KEY]});
        let original = v.clone();
        b.hide_value(&mut v);
        let text = v.to_string();
        assert!(!text.contains(KEY), "{text}");
        b.reveal_value(&mut v);
        assert_eq!(v, original);
    }

    #[test]
    fn nothing_recognised_means_nothing_changes() {
        let mut b = Bridge::new(rules());
        b.learn(b"{\"x\":\"hello\"}");
        assert!(b.is_empty());
        assert_eq!(b.hide("普通的一段话"), "普通的一段话");
    }

    #[test]
    fn the_tail_that_could_still_become_a_known_value_is_held() {
        let mut b = Bridge::new(rules());
        b.learn(format!("{{\"a\":\"{KEY}\"}}").as_bytes());
        let buf = format!("前面的话 {}", &KEY[..10]);
        assert_eq!(b.hold_from(&buf), buf.len() - 10);
        assert_eq!(b.hold_from("别的 sk"), "别的 sk".len() - 2);
        assert_eq!(b.hold_from("什么都不像"), "什么都不像".len());
    }

    /// 一本账里装着这些值（按出现的先后发号）
    fn bridge_with(values: &[&str]) -> Bridge {
        use tw_guard::redact::rules::{Hit, Rule};
        let text = values.join("\u{1}");
        let mut hits = Vec::new();
        let mut at = 0;
        for v in values {
            hits.push(Hit {
                bytes: at..at + v.len(),
                rule: Rule::Builtin("aws-access-key-id"),
                label: None,
            });
            at += v.len() + 1;
        }
        let ledger =
            tw_guard::redact::replace::apply(&text, &hits, Ledger::new(Scheme::SECRET)).ledger;
        Bridge::new(Arc::new(RuleSet::none())).with_ledger(ledger)
    }

    fn rng(mut seed: u64) -> impl FnMut(usize) -> usize {
        move |n| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        }
    }

    /// 一遍找出每一处再挑，和原来一个值一个值地整段 `replace`，换出来的一模一样：值互为
    /// 前缀、后缀、首尾相叠、自己和自己相叠、多字节的。值里没有占位符里会有的字（大写、
    /// 数字、`<>`），一个个换时不会在刚换上的占位符里再找到东西
    #[test]
    fn hiding_in_one_pass_writes_what_replacing_value_by_value_wrote() {
        let values = [
            "a", "ab", "ba", "aba", "abab", "bab", "abc", "ca", "中", "中文", "文中", "文a", "c中",
        ];
        let b = bridge_with(&values);
        let one_by_one = |s: &str| {
            let mut out = None::<String>;
            for (original, placeholder) in &b.values {
                let cur = out.as_deref().unwrap_or(s);
                if cur.contains(original.as_str()) {
                    out = Some(cur.replace(original.as_str(), placeholder));
                }
            }
            out.unwrap_or_else(|| s.to_string())
        };
        let alphabet = ["a", "b", "c", "中", "文", " ", "xyz"];
        let mut next = rng(0x2545_f491_4f6c_dd1d);
        for _ in 0..3000 {
            let s: String = (0..next(30))
                .map(|_| alphabet[next(alphabet.len())])
                .collect();
            assert_eq!(b.hide_known(&s), one_by_one(&s), "{s:?}");
        }
    }

    /// 二分查排好序的值，和拿每个值的每个真前缀去比，扣住的位置一模一样
    #[test]
    fn holding_back_agrees_with_trying_every_prefix_of_every_value() {
        let values = ["abab", "abc", "中文字", "a中", "bca", "sk-ant-api03-x"];
        let b = bridge_with(&values);
        let every = |buf: &str| {
            let mut cut = buf.len();
            for (original, _) in &b.values {
                let mut ends: Vec<usize> = original
                    .char_indices()
                    .map(|(i, _)| i)
                    .filter(|i| *i > 0)
                    .collect();
                ends.reverse();
                for k in ends {
                    if k <= buf.len() && buf.ends_with(&original[..k]) {
                        cut = cut.min(buf.len() - k);
                        break;
                    }
                }
            }
            cut
        };
        let alphabet = [
            "a", "b", "c", "中", "文", "字", " ", "sk-", "ant-", "api03-",
        ];
        let mut next = rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..3000 {
            let buf: String = (0..next(12))
                .map(|_| alphabet[next(alphabet.len())])
                .collect();
            assert_eq!(b.hold_from(&buf), every(&buf), "{buf:?}");
        }
        assert_eq!(Bridge::new(rules()).hold_from("abc"), 3, "空账什么都不扣");
    }

    /// 账里几万个值时，插件看到的请求照样换得过来：花的时间跟着请求的大小走，不跟着
    /// 「值的个数 × 字符串的个数」走
    #[test]
    fn a_request_with_tens_of_thousands_of_keys_is_hidden_in_linear_time() {
        let key = |i: usize| {
            let mut n = i;
            let tail: String = (0..16)
                .map(|_| {
                    let c = char::from(b'A' + (n % 26) as u8);
                    n /= 26;
                    c
                })
                .collect();
            format!("AKIA{tail}")
        };
        let round = |n: usize| {
            let messages: Vec<Value> = (0..n)
                .map(|i| serde_json::json!({"role": "user", "content": format!("key {}", key(i))}))
                .collect();
            let body = serde_json::json!({ "messages": messages });
            let started = std::time::Instant::now();
            let mut b = Bridge::new(rules());
            b.learn(body.to_string().as_bytes());
            let mut shown = body.clone();
            b.hide_value(&mut shown);
            let text = shown.to_string();
            assert!(!text.contains("AKIA"), "插件看到了真值");
            assert!(text.contains(&format!("<<TW_SECRET_{n}>>")));
            b.reveal_value(&mut shown);
            assert_eq!(shown, body);
            started.elapsed()
        };
        let fastest = |n| (0..2).map(|_| round(n)).min().unwrap();
        let (small, large) = (fastest(5_000), fastest(20_000));
        let ratio = large.as_secs_f64() / small.as_secs_f64();
        // 线性的是 4 倍上下；一个值一个值地找是 16 倍
        assert!(
            ratio < 10.0,
            "2 万个值花了 5 千个值的 {ratio:.1} 倍（{small:?} → {large:?}）"
        );
    }
}
