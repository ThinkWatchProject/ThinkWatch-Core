//! 拼上游地址。两个网关都用：base_url 的尾斜杠、查询串里的网关密钥，处理
//! 一处就够了。

/// 拼上游 URL。base_url 的尾斜杠在这里统一吃掉 —— 从浏览器地址栏粘一个
/// URL 就会白送一个尾斜杠，而 `https://host//v1/messages` 换来的是一个
/// 光秃秃的 404。查询串里的网关密钥去掉，见 [`without_gateway_key`]。
pub fn upstream_url(base_url: &str, path: &str, query: Option<&str>) -> String {
    let base = base_url.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    let query = query.map(without_gateway_key);
    match query {
        Some(q) if !q.is_empty() => format!("{base}/{path}?{q}"),
        _ => format!("{base}/{path}"),
    }
}

/// 去掉查询串里的 `key=`。
///
/// **那是网关密钥。**Gemini 的 REST 写法把密钥放在查询串里，网关认完身份之后
/// 原样把整个查询串拼到上游地址上，等于把它发给了上游。上游的凭据走请求头，
/// 用不着这一项。凡是把客户端的查询串转给上游的地方（HTTP、WS 升级）都要过它。
pub fn without_gateway_key(query: &str) -> String {
    query
        .split('&')
        .filter(|pair| *pair != "key" && !pair.starts_with("key="))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_slash_in_base_url_does_not_double_up() {
        // 从浏览器粘 URL 会白送一个尾斜杠，而 `//v1/messages` 换来 404。
        assert_eq!(
            upstream_url("https://api.example.com/", "/v1/messages", None),
            "https://api.example.com/v1/messages"
        );
        assert_eq!(
            upstream_url("https://api.example.com", "v1/messages", None),
            "https://api.example.com/v1/messages"
        );
    }

    #[test]
    fn query_is_carried_through_but_empty_query_adds_no_question_mark() {
        assert_eq!(
            upstream_url("https://x.com", "/v1/m", Some("beta=true")),
            "https://x.com/v1/m?beta=true"
        );
        assert_eq!(
            upstream_url("https://x.com", "/v1/m", Some("")),
            "https://x.com/v1/m"
        );
    }

    #[test]
    fn the_gateway_key_in_the_query_never_reaches_the_upstream() {
        assert_eq!(
            upstream_url(
                "https://g.example",
                "/v1beta/models/m:generateContent",
                Some("key=tw-secret&alt=sse")
            ),
            "https://g.example/v1beta/models/m:generateContent?alt=sse"
        );
        assert_eq!(
            upstream_url(
                "https://g.example",
                "/v1beta/models/m",
                Some("key=tw-secret")
            ),
            "https://g.example/v1beta/models/m"
        );
        // 名字只是以 key 开头的参数不受影响
        assert_eq!(
            upstream_url("https://g.example", "/x", Some("keyword=a")),
            "https://g.example/x?keyword=a"
        );
        assert_eq!(without_gateway_key("alt=sse&key"), "alt=sse");
    }
}
