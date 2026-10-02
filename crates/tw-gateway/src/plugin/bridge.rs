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

use std::sync::Arc;

use serde_json::Value;
use tw_guard::redact::replace::{Ledger, Scheme};
use tw_guard::redact::rules::RuleSet;

/// 一个请求的密钥映射。
#[derive(Clone)]
pub struct Bridge {
    rules: Arc<RuleSet>,
    ledger: Ledger,
    /// 账里的值，长的在前：换的时候长的先换，一个值是另一个的一部分时不会只换半截
    values: Vec<(String, String)>,
}

impl Bridge {
    pub fn new(rules: Arc<RuleSet>) -> Self {
        Self {
            rules,
            ledger: Ledger::new(Scheme::SECRET),
            values: Vec::new(),
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
            .map(|(o, p)| (o.to_string(), p.to_string()))
            .collect();
        values.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        self.values = values;
    }

    /// 一段文字里的密钥换成占位符：账里的值，加上按规则新找到的。
    pub fn hide(&mut self, s: &str) -> String {
        let mut out = None::<String>;
        for (original, placeholder) in &self.values {
            let cur = out.as_deref().unwrap_or(s);
            if cur.contains(original.as_str()) {
                out = Some(cur.replace(original.as_str(), placeholder));
            }
        }
        let cur = out.unwrap_or_else(|| s.to_string());
        if self.rules.is_empty() {
            return cur;
        }
        let mut hits = tw_guard::redact::rules::scan_text(&cur, &self.rules);
        // 压在一个占位符上的不算（连接串规则会把 `app:<<TW_SECRET_2>>@` 当成口令）
        if !hits.is_empty() && cur.contains(Scheme::SECRET.open) {
            let ours = Scheme::SECRET.find_in(&cur);
            hits.retain(|h| {
                !ours
                    .iter()
                    .any(|(at, _, _)| at.start < h.bytes.end && h.bytes.start < at.end)
            });
        }
        if hits.is_empty() {
            return cur;
        }
        let ledger = std::mem::replace(&mut self.ledger, Ledger::new(Scheme::SECRET));
        let r = tw_guard::redact::replace::apply(&cur, &hits, ledger);
        self.ledger = r.ledger;
        self.reindex();
        r.text
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
    pub fn hold_from(&self, buf: &str) -> usize {
        let mut cut = buf.len();
        for (original, _) in &self.values {
            // 从长到短试这个值的每一个真前缀
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
}
