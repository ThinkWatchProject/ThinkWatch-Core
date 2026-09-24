//! 防护：出站脱敏、对上游返回的工具调用的审查、藏在文本里的不可见字符、按关键词
//! 过滤请求，和回答的长度上限。
//!
//! **引擎在这里，规则集和配置各带各的。**桌面版脱的是开发者自己的 API key，
//! 企业版脱的是客户的身份证号和手机号；桌面版的规则写在 `config.yaml`，企业版
//! 的在系统设置里。两边共用的是「怎么找、怎么换、怎么在流里换回来、怎么审查
//! 一个工具调用」—— 这些和规则是什么、从哪来无关。

pub mod content;
pub mod hidden;
pub mod output;
pub mod redact;
pub mod tools;

/// 编一条调用方给的正则。**编译后的大小有上限**（NFA 和 DFA 各 1 MiB）——
/// 这条正则要在每个请求上跑，而 `(a|aa){200}` 这种写法在默认的 10 MiB 上限下
/// 要编好几秒、占几 MB，然后每个请求都付一遍。
fn bounded(pattern: &str) -> Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(pattern)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .build()
}
