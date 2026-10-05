//! 两个模型名说的是不是同一个模型。
//!
//! 上游体检拿发出去的模型名和上游在回答里写的比（见 `tw_store::health`），网关把回答里的
//! 模型名换回客户端用的名称之前也拿它比（见 `tw_gateway::answer_model`）。**两边写法
//! 不一样是常态**：请求里写别名、回答里写带日期的快照（`claude-sonnet-4-5` →
//! `claude-sonnet-4-5-20250929`），中转站带着厂商前缀（`anthropic/claude-sonnet-4.5`），
//! Bedrock 有它自己的一套（`us.anthropic.claude-sonnet-4-5-20250929-v1:0`）。
//!
//! **方向和价目表那边（[`crate::name`]）相反。**查价宁可查不到，也不能把一个模型
//! 猜成另一个、给出一个自信的错数字；这里宁可把两个名字认成同一个，也不能错判一次
//! 「对不上」—— 那是在冤枉一家诚实的上游。所以这里抹掉的是**一切不是换了模型的
//! 差别**，但模型本身的身份（家族、大小、代数、变体）一个字都不动：`opus` 和 `sonnet`、
//! `4` 和 `4-5`、`gpt-5` 和 `gpt-5-mini`、`qwen2.5:72b` 和 `qwen2.5:7b` 照样不一样。
//!
//! 同一个模型的两个快照（`claude-3-5-sonnet-20240620` 和 `-20241022`）归一之后是同一个：
//! 日期只说明是哪一天的版本，抹掉它是为了别名和快照能对上，代价是分不出快照之间的
//! 替换 —— 那比换了一个模型小得多。

/// 归一之后的名字。两个名字归一之后相等，就当是同一个模型。
///
/// 一步一步（顺序有关系）：
///
/// 1. 去掉首尾空白，转成小写
/// 2. 只留路径的最后一段：`models/`、厂商前缀（`anthropic/`、`openai/`、`google/`……）、
///    Bedrock 的 ARN、Vertex 的资源路径
/// 3. 去掉 `@` 和它后面的：Vertex 的版本（`@20250929`、`@001`、`@latest`）
/// 4. 去掉 OpenRouter 的变体后缀（`:free`、`:thinking`……，只认它列出的那几个词 ——
///    Ollama 的 `:7b` 是大小，不能去）
/// 5. Bedrock 的写法：推理配置文件的地域前缀（`us.`、`eu.`、`apac.`、`global.`……）、
///    厂商前缀（`anthropic.`……），以及厂商前缀后面才有的版本（`-v1:0`、`-v1`、`:200k`）
/// 6. 结尾的快照标记，可以叠着出现：`-latest`；日期（`-20250929`、`-2025-09-29`、
///    `-0613` 这种四位的月日或年月）；带日期的预览（`-preview-05-20`、`-exp-0827`，
///    不带日期的 `-preview` 不去：`o1-preview` 和 `o1` 是两个模型）
/// 7. 数字之间的点换成横线：`4.5` 和 `4-5` 是同一个版本的两种写法
/// 8. Claude 的两种命名顺序（`claude-3-5-sonnet` 和 `claude-sonnet-4-5`）排成一种；
///    中转站给 Claude 起的 `-thinking`、`-1m` 后缀去掉（Anthropic 没有叫这些名字的
///    模型，那是同一个模型上的开关）；小版本是 0 的（`claude-opus-4-0`）和没写小版本的
///    一样
///
/// 第 6 步还去掉结尾方括号里的标记（`[1m]`）：那不是模型 id 的一部分。
pub fn normalize(name: &str) -> String {
    let lower = name.trim().to_lowercase();
    // 2. 最后一段。结尾带着斜杠的（`anthropic/`）不算一段
    let mut s = lower
        .rsplit('/')
        .find(|seg| !seg.is_empty())
        .unwrap_or_default();
    // 3. Vertex 的版本
    if let Some((base, _)) = s.split_once('@') {
        s = base;
    }
    // 4. OpenRouter 的变体
    if let Some((base, tag)) = s.rsplit_once(':')
        && OPENROUTER_VARIANTS.contains(&tag)
    {
        s = base;
    }
    // 5. Bedrock
    let s = bedrock(s);
    // 6. 快照标记。叠着的（`-20250219-thinking`、`-preview-0827`）一层一层剥
    let mut s = s.to_string();
    loop {
        let before = s.len();
        // 方括号里的标记不是模型 id 的一部分：Claude Code 的 `[1m]` 说的是开着长上下文
        if s.ends_with(']')
            && let Some(i) = s.rfind('[')
        {
            s.truncate(i);
        }
        if let Some(base) = s.strip_suffix("-latest") {
            s.truncate(base.len());
        }
        if is_claude(&s)
            && let Some(base) = CLAUDE_ALIASES.iter().find_map(|a| s.strip_suffix(a))
        {
            s.truncate(base.len());
        }
        if let Some(len) = snapshot(&s) {
            s.truncate(len);
        }
        if s.len() == before {
            break;
        }
    }
    // 7. 数字之间的点
    let s = dots_between_digits(&s);
    // 8. Claude
    if is_claude(&s) {
        return claude(&s);
    }
    s
}

/// 两个名字是不是同一个模型。任何一边归一之后是空的，就说不上是不是 —— 当作对得上：
/// 说不上的不该算成一次「对不上」
pub fn same(a: &str, b: &str) -> bool {
    let (a, b) = (normalize(a), normalize(b));
    a.is_empty() || b.is_empty() || a == b
}

/// OpenRouter 的变体后缀：同一个模型、不同的走法（免费额度、推理、更快的服务商……）。
/// 取自它的文档；**别的冒号后缀不去**，Ollama 的 `qwen2.5:7b` 和 `:72b` 是两个模型
const OPENROUTER_VARIANTS: &[&str] = &[
    "free", "thinking", "nitro", "beta", "extended", "online", "floor", "exacto",
];

/// Bedrock 上的厂商前缀。**只认这几个**：`qwen2.5-72b` 的 `qwen2` 后面也是一个点，
/// 把「一段字母加一个点」都当前缀的话，它会被剥成 `5-72b`
const BEDROCK_VENDORS: &[&str] = &[
    "anthropic",
    "amazon",
    "meta",
    "mistral",
    "cohere",
    "ai21",
    "deepseek",
    "openai",
    "qwen",
    "writer",
    "moonshot",
    "minimax",
];

/// Bedrock 的写法剥成模型本来的名字：地域前缀、厂商前缀、版本。
///
/// 版本（`-v1:0`、`-v2:0`、`-v1`）**只在认出了厂商前缀之后才剥**：`deepseek-v3` 的 `-v3`
/// 是模型的代数，不是 Bedrock 的版本号。
fn bedrock(id: &str) -> &str {
    // 地域前缀：和价目表认的是同一张清单。后面得还是一个 `厂商.模型` 的 id
    let id = match id.split_once('.') {
        Some((geo, rest)) if crate::name::BEDROCK_GEOS.contains(&geo) && rest.contains('.') => rest,
        _ => id,
    };
    let Some(name) = id
        .split_once('.')
        .filter(|(vendor, rest)| BEDROCK_VENDORS.contains(vendor) && !rest.is_empty())
        .map(|(_, rest)| rest)
    else {
        return id;
    };
    // 上下文长度的变体：`…-v1:0:200k`
    let name = match name.rsplit_once(':') {
        Some((base, ctx)) if ctx.ends_with('k') && digits(&ctx[..ctx.len() - 1]) => base,
        _ => name,
    };
    let Some(i) = name.rfind("-v") else {
        return name;
    };
    let (major, minor) = match name[i + 2..].split_once(':') {
        Some((a, b)) => (a, Some(b)),
        None => (&name[i + 2..], None),
    };
    if digits(major) && minor.is_none_or(digits) {
        &name[..i]
    } else {
        name
    }
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())
}

/// 结尾的快照标记去掉之后剩多长；没有就是 None。
///
/// 认的形状（都在结尾，前面是 `-`）：
/// - 八位的日期 `20250929`
/// - `2025-09-29`
/// - 四位的月日（`0613`、`1106`）或者年月（`2507`）
/// - 预览加日期：`preview-05-20`、`preview-09-2025`、`exp-0827`
///
/// **每一种都要真是一个日期**：月份 01–12、日 01–31、年份 20xx / 2x（四位年月）。
/// `-8192`（Groq 写在名字里的上下文长度）、`-2048` 都不是，不去。
fn snapshot(s: &str) -> Option<usize> {
    // 结尾 n 个字节：只在它们全是 ASCII、前面一个字节是 `-` 时才切，切在字符边界上
    let tail = |n: usize| -> Option<&str> {
        let b = s.as_bytes();
        (b.len() > n && b[b.len() - n - 1] == b'-' && b[b.len() - n..].is_ascii())
            .then(|| &s[s.len() - n..])
    };
    let number = |x: &str, range: std::ops::RangeInclusive<u32>| {
        digits(x) && x.parse::<u32>().is_ok_and(|n| range.contains(&n))
    };
    let month = |m: &str| number(m, 1..=12);
    let day = |d: &str| number(d, 1..=31);
    let year = |y: &str| y.len() == 4 && y.starts_with("20") && digits(y);
    // 去掉 `-` 和后面 n 个字节之后剩多长
    let without = |n: usize| s.len() - n - 1;
    // 日期前面紧挨着 `preview` / `exp` 的，连它一起去
    let dated_preview = |len: usize| -> Option<usize> {
        ["-preview", "-exp"]
            .iter()
            .find_map(|m| s[..len].strip_suffix(m))
            .map(str::len)
    };
    let dated = if let Some(t) = tail(8)
        && year(&t[..4])
        && month(&t[4..6])
        && day(&t[6..])
    {
        Some(without(8))
    } else if let Some(t) = tail(10)
        && year(&t[..4])
        && &t[4..5] == "-"
        && month(&t[5..7])
        && &t[7..8] == "-"
        && day(&t[8..])
    {
        Some(without(10))
    } else if let Some(t) = tail(4)
        && ((month(&t[..2]) && day(&t[2..])) || (&t[..1] == "2" && digits(t) && month(&t[2..])))
    {
        Some(without(4))
    } else {
        None
    };
    if let Some(len) = dated {
        return Some(dated_preview(len).unwrap_or(len));
    }
    // 两段的日期（`-05-20`、`-09-2025`）**只在 `preview` / `exp` 后面认**：光是两段数字，
    // 可能是版本号
    if let Some(t) = tail(5)
        && month(&t[..2])
        && &t[2..3] == "-"
        && day(&t[3..])
    {
        return dated_preview(without(5));
    }
    if let Some(t) = tail(7)
        && month(&t[..2])
        && &t[2..3] == "-"
        && year(&t[3..])
    {
        return dated_preview(without(7));
    }
    None
}

/// `4.5` → `4-5`：只换两边都是数字的点
fn dots_between_digits(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    for (i, c) in s.char_indices() {
        let between = c == '.'
            && i > 0
            && b[i - 1].is_ascii_digit()
            && b.get(i + 1).is_some_and(u8::is_ascii_digit);
        out.push(if between { '-' } else { c });
    }
    out
}

/// Claude 的三个家族
const CLAUDE_FAMILIES: &[&str] = &["opus", "sonnet", "haiku"];

/// 中转站给 Claude 起的、说的是同一个模型的后缀：开着推理（`-thinking`）、开着百万
/// 上下文（`-1m`）。Anthropic 没有叫这些名字的模型，两样都是同一个模型上的开关
const CLAUDE_ALIASES: &[&str] = &["-thinking", "-1m"];

fn is_claude(s: &str) -> bool {
    s.starts_with("claude-")
}

/// Claude 的名字排成一种顺序：`claude-家族-版本-其余`。
///
/// Anthropic 自己就换过一次顺序：3.x 是 `claude-3-5-sonnet`，4 起是 `claude-sonnet-4-5`；
/// 中转站两种都写（`claude-4-sonnet`）。只在恰好有一个家族词时排，其余的词按原来的
/// 顺序跟在版本后面 —— 家族和版本一个都不丢，排的只是顺序。小版本是 0 的去掉：
/// `claude-opus-4-0` 是 `claude-opus-4` 的别名。
fn claude(s: &str) -> String {
    let words: Vec<&str> = s.split('-').skip(1).collect();
    let families: Vec<&str> = words
        .iter()
        .copied()
        .filter(|w| CLAUDE_FAMILIES.contains(w))
        .collect();
    let [family] = families.as_slice() else {
        return s.to_string();
    };
    let mut version: Vec<&str> = words.iter().copied().filter(|w| digits(w)).collect();
    let rest = words.iter().copied().filter(|w| w != family && !digits(w));
    if version.len() >= 2 && version.last() == Some(&"0") {
        version.pop();
    }
    std::iter::once("claude")
        .chain(std::iter::once(*family))
        .chain(version)
        .chain(rest)
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 不是换了模型的差别：**这些都必须认成同一个**。错判一次就是冤枉一家诚实的上游
    #[test]
    fn the_same_model_written_differently_is_the_same() {
        for (sent, answered) in [
            // 大小写、空白
            ("Claude-Sonnet-4-5", "claude-sonnet-4-5"),
            (" gpt-4o ", "gpt-4o"),
            // Anthropic：别名 → 快照、-latest、小版本 0
            ("claude-sonnet-4-5", "claude-sonnet-4-5-20250929"),
            ("claude-3-5-sonnet-latest", "claude-3-5-sonnet-20241022"),
            ("claude-opus-4-0", "claude-opus-4-20250514"),
            ("claude-sonnet-4", "claude-sonnet-4-0"),
            // Vertex 的 @ 版本
            ("claude-sonnet-4-5", "claude-sonnet-4-5@20250929"),
            ("gemini-1.5-pro", "gemini-1.5-pro@002"),
            (
                "publishers/anthropic/models/claude-sonnet-4-5@20250929",
                "claude-sonnet-4-5",
            ),
            // OpenAI：别名 → 快照（ISO 日期、四位月日）
            ("gpt-4o", "gpt-4o-2024-08-06"),
            ("gpt-5", "gpt-5-2025-08-07"),
            ("o3", "o3-2025-04-16"),
            ("gpt-4.1-mini", "gpt-4.1-mini-2025-04-14"),
            ("gpt-3.5-turbo", "gpt-3.5-turbo-0125"),
            ("gpt-4", "gpt-4-0613"),
            ("chatgpt-4o-latest", "chatgpt-4o-latest"),
            // 四位年月
            ("qwen3-235b-a22b-instruct-2507", "qwen3-235b-a22b-instruct"),
            ("deepseek-r1", "deepseek-r1-0528"),
            // models/ 和厂商前缀、多段的前缀
            ("gemini-2.5-pro", "models/gemini-2.5-pro"),
            ("claude-sonnet-4-5", "anthropic/claude-sonnet-4.5"),
            ("gpt-4o-mini", "openai/gpt-4o-mini"),
            ("gemini-2.5-flash", "google/gemini-2.5-flash"),
            ("deepseek-chat", "deepseek/deepseek-chat"),
            (
                "claude-sonnet-4-5",
                "openrouter/anthropic/claude-sonnet-4-5-20250929",
            ),
            // OpenRouter 的变体
            ("deepseek-r1", "deepseek/deepseek-r1:free"),
            ("claude-3-7-sonnet", "anthropic/claude-3.7-sonnet:thinking"),
            // Bedrock：推理配置文件、厂商前缀、版本、ARN
            (
                "claude-sonnet-4-5",
                "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            ),
            (
                "claude-sonnet-4-5-20250929",
                "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
            ),
            (
                "claude-3-5-sonnet-20241022",
                "anthropic.claude-3-5-sonnet-20241022-v2:0",
            ),
            ("claude-opus-4-6", "apac.anthropic.claude-opus-4-6-v1"),
            (
                "claude-3-sonnet",
                "anthropic.claude-3-sonnet-20240229-v1:0:200k",
            ),
            (
                "arn:aws:bedrock:us-east-1:123456789012:inference-profile/eu.anthropic.claude-haiku-4-5-20251001-v1:0",
                "claude-haiku-4-5",
            ),
            ("amazon.nova-pro-v1:0", "nova-pro"),
            (
                "us.meta.llama3-1-70b-instruct-v1:0",
                "llama3-1-70b-instruct",
            ),
            // 点和横线
            ("claude-sonnet-4.5", "claude-sonnet-4-5"),
            ("glm-4.6", "glm-4-6"),
            // Claude 的两种命名顺序
            ("claude-3-5-sonnet-20240620", "claude-sonnet-3-5"),
            ("claude-4-sonnet", "claude-sonnet-4-20250514"),
            ("claude-4.5-sonnet", "claude-sonnet-4-5"),
            ("claude-3-opus-20240229", "claude-opus-3"),
            // 中转站给开了推理、开了长上下文的 Claude 起的名字
            ("claude-sonnet-4-5-thinking", "claude-sonnet-4-5-20250929"),
            ("claude-3-7-sonnet-20250219-thinking", "claude-3-7-sonnet"),
            ("claude-sonnet-4-5-1m", "claude-sonnet-4-5-20250929"),
            ("claude-sonnet-4-5-20250929[1m]", "claude-sonnet-4-5"),
            // 带日期的预览
            ("gemini-2.5-flash-preview-05-20", "gemini-2.5-flash"),
            ("gemini-2.5-flash-preview-09-2025", "gemini-2.5-flash"),
            ("gemini-1.5-flash-exp-0827", "gemini-1.5-flash"),
            ("gemini-flash-latest", "gemini-flash"),
        ] {
            assert!(
                same(sent, answered),
                "{sent} 和 {answered} 是同一个模型：{} vs {}",
                normalize(sent),
                normalize(answered)
            );
            assert!(same(answered, sent), "{answered} / {sent} 反过来比不一样");
        }
    }

    /// 真换了模型的：家族、大小、代数、变体，**一个都不能被归一抹掉**
    #[test]
    fn a_different_model_stays_different() {
        for (sent, answered) in [
            ("claude-opus-4-1", "claude-sonnet-4-5-20250929"),
            ("claude-sonnet-4-5", "claude-sonnet-4"),
            ("claude-sonnet-4-5", "claude-haiku-4-5"),
            ("claude-3-5-sonnet", "claude-3-5-haiku"),
            ("claude-opus-4-1", "claude-opus-4"),
            ("gpt-5", "gpt-5-mini"),
            ("gpt-4o", "gpt-4o-mini-2024-07-18"),
            ("gpt-4.1", "gpt-4.1-nano"),
            ("gpt-5", "gpt-5-chat-latest"),
            ("chatgpt-4o-latest", "gpt-4o"),
            ("o1", "o1-preview"),
            ("o3", "o3-mini"),
            ("gemini-2.5-pro", "gemini-2.5-flash"),
            ("gemini-2.5-flash", "gemini-2.5-flash-lite"),
            ("gemini-2.0-flash", "gemini-2.0-flash-exp"),
            ("deepseek-reasoner", "deepseek-chat"),
            ("deepseek-v3", "deepseek-v2"),
            ("deepseek-v3.1", "deepseek-v3"),
            ("qwen2.5:72b", "qwen2.5:7b"),
            (
                "qwen3-235b-a22b-thinking-2507",
                "qwen3-235b-a22b-instruct-2507",
            ),
            ("kimi-k2-thinking", "kimi-k2"),
            ("glm-4.6", "glm-4.5"),
            ("glm-4.5", "glm-4.5-air"),
            // 不认识的前缀不剥：`xx.` 不是 Bedrock 的地域
            ("claude-opus-4-7", "xx.claude-opus-4-8"),
            // 带着上下文长度的数字不是日期
            ("llama3-70b-8192", "llama3-8b-8192"),
        ] {
            assert!(
                !same(sent, answered),
                "{sent} 和 {answered} 不是同一个模型，却归一成了 {}",
                normalize(sent)
            );
        }
    }

    #[test]
    fn what_a_name_normalizes_to() {
        for (name, want) in [
            ("claude-sonnet-4-5-20250929", "claude-sonnet-4-5"),
            (
                "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
                "claude-sonnet-4-5",
            ),
            ("claude-3-5-sonnet-20241022", "claude-sonnet-3-5"),
            ("anthropic/claude-3.7-sonnet:thinking", "claude-sonnet-3-7"),
            ("models/gemini-2.5-pro", "gemini-2-5-pro"),
            ("gpt-4o-2024-08-06", "gpt-4o"),
            ("deepseek-chat", "deepseek-chat"),
            // 没有日期形状的数字不动
            ("gpt-4o-2024", "gpt-4o-2024"),
            ("llama3-70b-8192", "llama3-70b-8192"),
            ("gpt-4-1106-preview", "gpt-4-1106-preview"),
            // 不像 Bedrock id 的点不当前缀
            ("qwen2.5-72b-instruct", "qwen2-5-72b-instruct"),
            ("deepseek-v3", "deepseek-v3"),
        ] {
            assert_eq!(normalize(name), want, "{name}");
        }
    }

    /// 说不上是不是的（空的、只有前缀的）：不算对不上
    #[test]
    fn an_empty_name_is_not_a_mismatch() {
        assert!(same("", "claude-sonnet-4-5"));
        assert!(same("gpt-4o", "   "));
        assert!(same("/", "gpt-4o"));
        // 结尾的斜杠不算一段：`anthropic/` 剩下的是 `anthropic`
        assert_eq!(normalize("anthropic/"), "anthropic");
        assert_eq!(normalize(""), "");
    }

    /// **模型名会是中转站自己起的中文**，而这个函数按字节找分隔符：切在多字节字符中间
    /// 会 panic。这个项目在模型名上已经栽过两次了
    #[test]
    fn a_name_with_multibyte_characters_does_not_panic() {
        for m in [
            "某个中转站自己起的名字",
            "中转-20250101",
            "こんにちは-2025-09-29",
            "🙂-0613",
            "claude-中-sonnet",
            "us.anthropic.中-v1:中",
            "anthropic.claude-中-v中",
            "中.5",
            "a中",
            "———————————",
            "预览-preview-05-20",
            "模型@版本",
        ] {
            let n = normalize(m);
            assert!(same(m, m), "{m}");
            let _ = n;
        }
        assert_eq!(normalize("中转专用-20250101"), "中转专用");
    }
}
