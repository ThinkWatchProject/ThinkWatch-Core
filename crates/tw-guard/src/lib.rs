//! 防护：出站脱敏、对上游返回的工具调用的审查、按规则过滤调用方发来的正文，和两样
//! 暂时还在的旧东西（藏在文本里的不可见字符的请求检查、回答的长度上限）。
//!
//! **两个产品的规则模型只在这里定义一份。**档位、内置规则目录、启停、处置、自定义规则、
//! 校验（[`policy`]），规则视图（[`view`]）和「测试…」（[`trial`]）：桌面版写进
//! `config.yaml`，企业版存进系统设置，结构相同，管理接口返回同一份 JSON。两边各管的只是
//! 存在哪儿、错误怎么报、命中之后记在哪儿。
//!
//! 引擎也在这里：「怎么找、怎么换、怎么在流里换回来」（[`redact`]），「怎么审查一个工具
//! 调用」（[`tools`]），「查调用方的哪些正文、怎么匹配、怎么删」（[`content`]）。

pub mod content;
pub mod hidden;
pub mod output;
pub mod policy;
pub mod redact;
pub mod tools;
pub mod trial;
pub mod view;

/// 编一条调用方给的正则。**编译后的大小有上限**（NFA 和 DFA 各 1 MiB）——
/// 这条正则要在每个请求上跑，而 `(a|aa){200}` 这种写法在默认的 10 MiB 上限下
/// 要编好几秒、占几 MB，然后每个请求都付一遍。
fn bounded(pattern: &str) -> Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(pattern)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .build()
}
