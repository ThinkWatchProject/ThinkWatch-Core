//! 模型名归一化。
//!
//! 客户端发过来的名字和价目表里的键**经常不是同一个字符串**：带日期
//! 后缀的（`claude-sonnet-4-5-20250929`）、带厂商前缀的
//! （`anthropic/claude-sonnet-4-5`）、中转站自己起的别名。
//!
//! **只做能证明是同一个模型的变换，一步都不多。**模糊匹配会把
//! `claude-opus-4-1-super` 猜成 `claude-opus-4`，然后给出一个自信的
//! 错误数字 —— 而那比不知道更糟。

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
    // GLM 在数据集里**只有带前缀的键**（`zai/glm-5.2`），而客户端发来的是裸名字
    // `glm-5.2` —— 不补这一个候选，走 Z.ai 和 BigModel 的每一条请求都是「价格未知」。
    //
    // **放在最后一位**：第三方中转上的同名模型价格和官方不同，前面那几个候选（原名、
    // 剥掉前缀的名字）先命中的算数，官方价只是兜底。中转的真实价格本来就得靠自定义
    // 价目表 —— 那是第二层该管的事。
    if let Some(glm) = zai_key(bare) {
        push(glm);
    }
    out
}

/// GLM 的官方价格在数据集里的键。**只认 `glm-` 开头** —— 别的名字没有这个规律，
/// 给它们凑一个前缀就是在猜
fn zai_key(bare: &str) -> Option<String> {
    bare.starts_with("glm-").then(|| format!("zai/{bare}"))
}

/// 跨平台借价：本名查不到时，同一个模型在别的平台上的写法。按「越接近越靠前」排，
/// 调用方依次查表，第一个命中的算数。
///
/// **借来的价格都不是精确的。**本来以为「Bedrock 的单价和直连一样」，一条覆盖
/// 整份快照的测试当场证伪了：`claude-3-haiku-20240307` 的输入输出价一样，
/// **缓存价差 17%**（读 3e-8 vs 2.5e-8，写 3e-7 vs 3.125e-7）；Claude 4.5 起
/// Bedrock 的地域推理配置文件（`us.`、`eu.`……）还比全球的贵 10%。
///
/// 所以它和 [`candidates`] 分开、单独返回：用它算出来的成本一律标成**估算**
/// （估算值必须在界面上明确标记）。给一个带波浪号的数字，比给一个空白有用；
/// 而假装它精确，就是那种「看起来很确定的错数字」。
///
/// 三种借法，都只对能证明是同一个模型的写法：
///
/// 1. Bedrock 推理配置文件 → 它以之命名的那个模型 id：`us.anthropic.claude-…` →
///    `anthropic.claude-…`
/// 2. Bedrock 上 Anthropic 模型的 id → Anthropic 自己的名字：
///    `anthropic.claude-sonnet-4-5-20250929-v1:0` → `claude-sonnet-4-5-20250929`
/// 3. Anthropic 的名字 → Bedrock 的 id。数据集里有些模型只有 Bedrock 的键 ——
///    `claude-3-5-haiku-20241022` 就是一个，而那是 Claude Code 真的会发的模型
///
/// 只对 Anthropic 的模型做 2 和 3。Vertex 的价格差得更多（`vertex_ai/claude-3-5-haiku`
/// 比 Bedrock 贵 25%），别家的模型在 Bedrock 上和直连的名字也对不上 —— 分不出的
/// 时候就别猜。
pub fn cross_platform(model: &str) -> Vec<String> {
    let mut out = Vec::new();
    // ARN 的最后一段就是模型 id 或推理配置文件 id
    let Some(bare) = model.trim().rsplit('/').next().filter(|b| !b.is_empty()) else {
        return out;
    };
    let mut push = |s: String| {
        if !out.contains(&s) {
            out.push(s);
        }
    };
    let id = strip_geo(bare);
    if let Some(id) = id {
        push(id.to_string());
    }
    if let Some(direct) = anthropic_on_bedrock(id.unwrap_or(bare)) {
        for c in candidates(direct) {
            push(c);
        }
    }
    if bare.starts_with("claude-") {
        push(format!("anthropic.{bare}-v1:0"));
    }
    out
}

/// Bedrock 跨区域推理配置文件的地域前缀。配置文件以它支持的模型命名，前面加
/// 一段地域：`us.anthropic.claude-…` 就是 `anthropic.claude-…` 在美国几个区域
/// 之间调度。
///
/// **只认这几个**，取自价格数据集里 Bedrock 的键。认不出的前缀不剥 —— 查不到价
/// 就说查不到。上游体检比模型名时认的也是这一张（[`crate::model_name`]）
pub const BEDROCK_GEOS: &[&str] = &["us", "eu", "apac", "jp", "au", "us-gov", "global"];

/// 剥掉推理配置文件的地域前缀，剩下的是模型 id。不是配置文件的 `None`。
fn strip_geo(id: &str) -> Option<&str> {
    let (geo, rest) = id.split_once('.')?;
    // 剩下的得还是一个 `厂商.模型` 的 id：`us.x` 这种不是
    (BEDROCK_GEOS.contains(&geo) && rest.contains('.')).then_some(rest)
}

/// Bedrock 上 Anthropic 模型的 id 去掉厂商和版本，剩下 Anthropic 自己的名字。
///
/// 版本后缀有三种写法：`-v1:0`、`-v1`（`claude-opus-4-6-v1`）、没有（`claude-opus-4-7`）。
fn anthropic_on_bedrock(id: &str) -> Option<&str> {
    let name = id.strip_prefix("anthropic.")?;
    if !name.starts_with("claude-") {
        return None;
    }
    // `rfind` 落在 ASCII 的 `-` 上，是字符边界；版本号只认 ASCII 数字和冒号
    let Some(i) = name.rfind("-v") else {
        return Some(name);
    };
    let (major, minor) = match name[i + 2..].split_once(':') {
        Some((a, b)) => (a, Some(b)),
        None => (&name[i + 2..], None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    if digits(major) && minor.is_none_or(digits) {
        Some(&name[..i])
    } else {
        Some(name)
    }
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
    // 这个坑这个项目已经栽过两次了，所以这里连一次 `&s[..]` 都
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
    fn a_glm_model_also_gets_looked_up_under_its_vendor_prefix() {
        // 数据集里只有 `zai/glm-…`。客户端发来的是裸名字，**不补这一个候选，
        // 走 Z.ai 的请求就全是「价格未知」**。
        let c = candidates("glm-4.6");
        assert_eq!(c[0], "glm-4.6", "先按原样查");
        assert_eq!(c.last().unwrap(), "zai/glm-4.6", "官方价只是兜底，排在最后");
        // 别的名字不凑前缀
        assert!(!candidates("kimi-k2").iter().any(|c| c.starts_with("zai/")));
    }

    #[test]
    fn both_a_prefix_and_a_date_can_be_stripped_together() {
        let c = candidates("openrouter/anthropic/claude-sonnet-4-5-20250929");
        assert!(c.contains(&"claude-sonnet-4-5".to_string()), "{c:?}");
    }

    #[test]
    fn a_number_that_is_not_a_date_is_left_alone() {
        // **`gpt-4o-2024` 剥成 `gpt-4o` 是猜。**猜出来的价格看起来是个
        // 确定的数字，而那正是最该避免的。
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
        assert!(cross_platform("").is_empty());
        assert!(cross_platform("arn:aws:bedrock:us-east-1:1:inference-profile/").is_empty());
    }

    #[test]
    fn an_inference_profile_borrows_from_its_model_then_from_anthropic() {
        assert_eq!(
            cross_platform("us.anthropic.claude-sonnet-4-5-20250929-v1:0"),
            [
                "anthropic.claude-sonnet-4-5-20250929-v1:0",
                "claude-sonnet-4-5-20250929",
                "claude-sonnet-4-5",
            ]
        );
        // 系统定义的配置文件的 ARN：最后一段就是它的 id
        assert_eq!(
            cross_platform(
                "arn:aws:bedrock:us-east-1:123456789012:inference-profile/global.anthropic.claude-opus-4-6-v1"
            )[..2],
            ["anthropic.claude-opus-4-6-v1", "claude-opus-4-6"]
        );
    }

    #[test]
    fn every_way_bedrock_writes_a_version_comes_off() {
        for (id, name) in [
            (
                "anthropic.claude-sonnet-4-5-20250929-v1:0",
                "claude-sonnet-4-5-20250929",
            ),
            (
                "anthropic.claude-3-5-sonnet-20241022-v2:0",
                "claude-3-5-sonnet-20241022",
            ),
            ("anthropic.claude-opus-4-6-v1", "claude-opus-4-6"),
            ("anthropic.claude-opus-4-7", "claude-opus-4-7"),
        ] {
            assert_eq!(cross_platform(id)[0], name, "{id}");
        }
    }

    #[test]
    fn only_known_geographies_and_only_anthropic_models_are_translated() {
        // 认不出的前缀不剥
        assert!(cross_platform("xx.anthropic.claude-opus-4-7").is_empty());
        // 别家的模型：剥得掉地域，但不换成直连的名字
        assert_eq!(
            cross_platform("us.amazon.nova-pro-v1:0"),
            ["amazon.nova-pro-v1:0"]
        );
        assert!(cross_platform("amazon.nova-pro-v1:0").is_empty());
        assert!(cross_platform("gpt-4o").is_empty());
        assert!(cross_platform("gemini-2.5-pro").is_empty());
        // 厂商不是地域：`anthropic.` 不剥成别的 id，只换成直连的名字
        assert_eq!(
            cross_platform("anthropic.claude-opus-4-7"),
            ["claude-opus-4-7"]
        );
        // 直连的名字 → Bedrock 的键
        assert_eq!(cross_platform("claude-x"), ["anthropic.claude-x-v1:0"]);
    }
}

#[cfg(test)]
mod multibyte_tests {
    use super::*;

    /// **这个函数被每一个请求的模型名调到**，而中转站完全可能起一个中文
    /// 名字。按字节切 `&str` 在那时会 panic —— 这个项目已经栽过两次了，
    /// 第三次是这条测试当场抓住的。
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
            // 跨平台借价也会被每个查不到价的模型名调到
            cross_platform(m);
            cross_platform(&format!("us.anthropic.{m}-v中"));
            cross_platform(&format!("anthropic.claude-{m}-v1:中"));
        }
    }

    #[test]
    fn a_date_suffix_after_chinese_is_still_stripped() {
        let c = candidates("中转专用-20250101");
        assert!(c.contains(&"中转专用".to_string()), "{c:?}");
    }
}
