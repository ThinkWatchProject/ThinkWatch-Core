//! 模型名归一化（DESIGN.md §4.3.0）。
//!
//! 客户端发过来的名字和价目表里的键**经常不是同一个字符串**：带日期
//! 后缀的（`claude-sonnet-4-5-20250929`）、带厂商前缀的
//! （`anthropic/claude-sonnet-4-5`）、中转站自己起的别名。
//!
//! **只做能证明是同一个模型的变换，一步都不多。**模糊匹配会把
//! `claude-opus-4-1-super` 猜成 `claude-opus-4`，然后给出一个自信的
//! 错误数字 —— 而 §4.3 说得很清楚：那比不知道更糟。

/// 候选名，按「越确定越靠前」排。调用方依次查表，第一个命中的算数。
pub fn candidates(model: &str) -> Vec<String> {
    let mut out = Vec::new();
    let m = model.trim();
    if m.is_empty() {
        return out;
    }
    let mut push = |s: String| {
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    };
    push(m.to_string());

    // 厂商前缀。`anthropic/claude-…`、`openrouter/anthropic/claude-…`
    // 都在数据集里各有各的键，所以**先按原样查**，查不到再剥。
    let bare = m.rsplit('/').next().unwrap_or(m);
    push(bare.to_string());

    // 日期后缀。`-20250929` / `-2025-09-29` / `@20250929`
    for base in [m, bare] {
        if let Some(stripped) = strip_date_suffix(base) {
            push(stripped.to_string());
        }
    }
    // 剥完日期再剥一次前缀（`anthropic/claude-x-20250101`）
    if let Some(stripped) = strip_date_suffix(m) {
        push(stripped.rsplit('/').next().unwrap_or(stripped).to_string());
    }
    out
}

/// 跨平台的最后一招：Bedrock 的写法。
///
/// 数据集里有些模型只有 `anthropic.<名字>-v1:0` 这个键，没有裸名字 ——
/// `claude-3-5-haiku-20241022` 就是一个，而那是 Claude Code 真的会发的
/// 模型。查不到它意味着那部分流量的成本整个是空的。
///
/// **但这不是等价替换。**本来以为「Bedrock 的单价和直连一样」，一条覆盖
/// 整份快照的测试当场证伪了：`claude-3-haiku-20240307` 的输入输出价一样，
/// **缓存价差 17%**（读 3e-8 vs 2.5e-8，写 3e-7 vs 3.125e-7）。
///
/// 所以它单独成一个函数、单独返回：用它算出来的成本一律标成**估算**
/// （§4.3：估算值必须在界面上明确标记）。给一个带波浪号的数字，比给一个
/// 空白有用；而假装它精确，就是那种「看起来很确定的错数字」。
pub fn cross_platform_fallback(model: &str) -> Option<String> {
    let bare = model.trim().rsplit('/').next()?;
    // 只对 Anthropic 的模型。Vertex 的价格差得更多
    // （`vertex_ai/claude-3-5-haiku` 比 Bedrock 贵 25%），而按名字根本
    // 分不出用户走的是哪条路 —— 分不出的时候就别猜。
    bare.starts_with("claude-")
        .then(|| format!("anthropic.{bare}-v1:0"))
}

/// 结尾是不是一个日期版本号，是就剥掉。
///
/// **只认「八位数字」和「四位-两位-两位」两种形状。**别的都可能是模型
/// 名字本身的一部分 —— `gpt-4o-2024` 剥成 `gpt-4o` 是猜，而
/// `claude-3-5-sonnet-20241022` 剥成 `claude-3-5-sonnet` 是事实。
fn strip_date_suffix(s: &str) -> Option<&str> {
    for sep in ['-', '@'] {
        // `rfind` 给的是字节下标，而它落在一个 ASCII 分隔符上 —— 那一定
        // 是字符边界，所以下面这两次切是安全的。
        if let Some(i) = s.rfind(sep) {
            let tail = &s[i + 1..];
            if tail.len() == 8 && tail.bytes().all(|c| c.is_ascii_digit()) {
                return Some(&s[..i]);
            }
        }
    }
    // `-2025-09-29`。**按字节切 &str 会 panic** —— 模型名里完全可能有
    // 中文（中转站自己起的名字），而这个函数会被每一个请求的模型名调到。
    // 这个坑这个项目已经栽过两次了（§9.7），所以这里连一次 `&s[..]` 都
    // 不写：只在确认结尾 11 个**字节**全是 ASCII 之后才切。
    let b = s.as_bytes();
    if b.len() > 11 {
        let t = &b[b.len() - 11..];
        if t[0] == b'-'
            && t[5] == b'-'
            && t[8] == b'-'
            && t.iter()
                .enumerate()
                .all(|(i, c)| matches!(i, 0 | 5 | 8) || c.is_ascii_digit())
        {
            // 结尾这 11 个字节全是 ASCII，所以 len-11 一定是字符边界
            return Some(&s[..s.len() - 11]);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exact_name_is_always_the_first_candidate() {
        // 价目表里有它就用它，别的变换一步都不该发生。
        assert_eq!(candidates("claude-sonnet-4-5")[0], "claude-sonnet-4-5");
    }

    #[test]
    fn a_date_suffix_is_stripped() {
        let c = candidates("claude-sonnet-4-5-20250929");
        assert_eq!(c[0], "claude-sonnet-4-5-20250929", "先按原样查");
        assert!(c.contains(&"claude-sonnet-4-5".to_string()), "{c:?}");
    }

    #[test]
    fn a_vendor_prefix_is_stripped_but_only_after_trying_the_full_name() {
        // `anthropic/claude-…` 在数据集里是个独立的键，而且价格可能和
        // 裸名字不一样（比如经 bedrock 的）。**先查全名。**
        let c = candidates("anthropic/claude-sonnet-4-5");
        assert_eq!(c[0], "anthropic/claude-sonnet-4-5");
        assert!(c.contains(&"claude-sonnet-4-5".to_string()), "{c:?}");
    }

    #[test]
    fn both_a_prefix_and_a_date_can_be_stripped_together() {
        let c = candidates("openrouter/anthropic/claude-sonnet-4-5-20250929");
        assert!(c.contains(&"claude-sonnet-4-5".to_string()), "{c:?}");
    }

    #[test]
    fn a_number_that_is_not_a_date_is_left_alone() {
        // **`gpt-4o-2024` 剥成 `gpt-4o` 是猜。**猜出来的价格看起来是个
        // 确定的数字，而那正是 §4.3 最反对的。
        let c = candidates("gpt-4o-2024");
        assert!(!c.contains(&"gpt-4o".to_string()), "{c:?}");
        let c = candidates("claude-opus-4-1");
        assert!(!c.contains(&"claude-opus-4".to_string()), "{c:?}");
    }

    #[test]
    fn the_iso_style_date_suffix_works_too() {
        let c = candidates("gpt-5-2025-09-29");
        assert!(c.contains(&"gpt-5".to_string()), "{c:?}");
    }

    #[test]
    fn no_candidate_is_ever_repeated() {
        // 重复只会让调用方多查几次表，但它说明我的变换在原地打转。
        let c = candidates("claude-sonnet-4-5");
        let mut sorted = c.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), c.len(), "{c:?}");
    }

    #[test]
    fn an_empty_or_whitespace_model_yields_nothing() {
        assert!(candidates("").is_empty());
        assert!(candidates("   ").is_empty());
    }
}

#[cfg(test)]
mod multibyte_tests {
    use super::*;

    /// **这个函数被每一个请求的模型名调到**，而中转站完全可能起一个中文
    /// 名字。按字节切 `&str` 在那时会 panic —— 这个项目已经栽过两次了
    /// （§9.7），第三次是这条测试当场抓住的。
    #[test]
    fn a_model_name_with_multibyte_characters_does_not_panic() {
        for m in [
            "某个中转站自己起的名字",
            "模型",
            "中转-20250101",
            "こんにちは-2025-09-29",
            "🙂-20250101",
            "a中",
            "中",
            "———————————",
        ] {
            let c = candidates(m);
            assert!(!c.is_empty(), "{m}");
            assert_eq!(c[0], m);
        }
    }

    #[test]
    fn a_date_suffix_after_chinese_is_still_stripped() {
        let c = candidates("中转专用-20250101");
        assert!(c.contains(&"中转专用".to_string()), "{c:?}");
    }
}
