//! 官方接口的事实：哪些是厂商官方的端点、它们要的请求头、写不出最大输出时
//! 用多少。
//!
//! 两个网关都要这几条，而它们各自抄一份的结果是一边比另一边弱（官方域名少几家、
//! 地址解析能被 `@` 骗过去、最大输出写死一个数）。

/// 转成 Anthropic 格式时必须带的 `anthropic-version`。上游配置里写了同名头的话
/// 以配置为准，这里只是没写时的那个值。
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// 厂商官方端点的域名。**只比 host，不看名字**：一个中转站可以把自己叫做
/// `anthropic-official`，但它没法让自己的地址变成 `api.anthropic.com`。
const OFFICIAL: &[&str] = &[
    "api.anthropic.com",
    "api.openai.com",
    "generativelanguage.googleapis.com",
    "api.x.ai",
    "api.deepseek.com",
    "api.moonshot.cn",
    "open.bigmodel.cn",
    "api.z.ai",
    "dashscope.aliyuncs.com",
    "chatgpt.com",
];

/// 这个地址是不是厂商官方的端点。格式转换按它决定要不要照官方接口的严格要求来写
/// 请求。`*.amazonaws.com`（Bedrock）也算。
///
/// **在 host 上比，不是在字符串里找。**`https://evil.com/api.anthropic.com/` 里也
/// 「含有」那个域名；`https://api.anthropic.com@evil.com/` 连过去的是 `evil.com`
/// （`@` 前面是用户名）；`https://evil.com?@api.anthropic.com` 的 host 也是
/// `evil.com`（`?` 起是查询串）。
pub fn is_official_host(url: &str) -> bool {
    let host = host_of(url).to_ascii_lowercase();
    OFFICIAL.contains(&host.as_str()) || host.ends_with(".amazonaws.com")
}

/// 地址里的 host：去掉协议头、用户信息和端口，到路径、查询串、片段为止。
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    // WHATWG 的解析器把 `\` 也当成路径分隔符
    let authority = rest.split(['/', '?', '#', '\\']).next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    host_port.split(':').next().unwrap_or("")
}

/// 客户端没写最大输出、目标格式又必须写（Anthropic）时，**按模型名**兜底用多少。
///
/// 知道这个模型真实上限的（价目表里有）应该先用那个：写大了上游拒绝，写小了回答
/// 被截断。这里是查不到时的退路：Claude 4 系列的上限都不低于 32000，Anthropic
/// 兼容接口背后的别家模型（DeepSeek 这类）多在 8192。
pub fn fallback_max_output_tokens(model: &str) -> u64 {
    if model.to_ascii_lowercase().contains("claude") {
        32000
    } else {
        8192
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_official_endpoint_is_recognised_by_its_host() {
        assert!(is_official_host("https://api.anthropic.com"));
        assert!(is_official_host("https://API.Anthropic.com:443/v1/"));
        assert!(is_official_host("https://api.deepseek.com/anthropic"));
        assert!(is_official_host(
            "https://bedrock-runtime.us-east-1.amazonaws.com"
        ));
        assert!(!is_official_host("https://relay.example.com"));
    }

    #[test]
    fn a_relay_cannot_dress_up_as_an_official_host() {
        for spoof in [
            "https://evil.com/api.anthropic.com/v1",
            "https://api.anthropic.com.evil.com",
            "https://api.anthropic.com@evil.com/v1",
            "https://evil.com?@api.anthropic.com",
            "https://evil.com#@api.anthropic.com",
            "https://evil.com\\@api.anthropic.com",
            "https://evilamazonaws.com",
        ] {
            assert!(!is_official_host(spoof), "{spoof}");
        }
        // 用户信息里放什么都不影响真正的 host
        assert!(is_official_host("https://user:pw@api.openai.com/v1"));
    }

    #[test]
    fn the_fallback_output_limit_goes_by_the_model_name() {
        assert_eq!(fallback_max_output_tokens("Claude-Opus-4-5"), 32000);
        assert_eq!(fallback_max_output_tokens("deepseek-chat"), 8192);
    }
}
