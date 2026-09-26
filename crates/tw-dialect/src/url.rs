//! 拼上游地址。两个网关都用：base_url 的尾斜杠、查询串里的网关密钥，处理
//! 一处就够了。

/// 拼上游 URL。base_url 的尾斜杠在这里统一吃掉 —— 从浏览器地址栏粘一个
/// URL 就会白送一个尾斜杠，而 `https://host//v1/messages` 换来的是一个
/// 光秃秃的 404。查询串里的网关密钥去掉，见 [`without_gateway_key`]。
///
/// **地址以版本段结尾、路径又以同一段开头时，这一段只留一个。**服务商文档给的
/// 地址常常带着版本段（`https://api.openai.com/v1`），客户端发来的路径也带着
/// （`/v1/chat/completions`）—— 照拼就是 `…/v1/v1/chat/completions`，一个 404。
/// 只认版本段（`v1`、`v1beta`）：别的段恰好同名，说明不了它们是同一段。
pub fn upstream_url(base_url: &str, path: &str, query: Option<&str>) -> String {
    let base = base_url.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    let path = match last_path_segment(base).filter(|s| is_version(s)) {
        Some(v) => match path.strip_prefix(v) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => rest.trim_start_matches('/'),
            _ => path,
        },
        None => path,
    };
    let query = query.map(without_gateway_key);
    match query {
        Some(q) if !q.is_empty() => format!("{base}/{path}?{q}"),
        _ => format!("{base}/{path}"),
    }
}

/// 地址路径的最后一段。**只有主机、没有路径的地址没有这一段** —— `http://v1:8080`
/// 里的 `v1` 是主机名。
fn last_path_segment(base: &str) -> Option<&str> {
    let rest = &base[base.find("://")? + 3..];
    let path = &rest[rest.find('/')?..];
    path.rsplit('/').next().filter(|s| !s.is_empty())
}

/// `v1`、`v2`、`v1beta`、`v1alpha`、`v1beta1` 这样的版本段。
fn is_version(seg: &str) -> bool {
    let Some(rest) = seg.strip_prefix('v') else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let tail = &rest[digits..];
    let tail = ["alpha", "beta"]
        .iter()
        .find_map(|w| tail.strip_prefix(w))
        .unwrap_or(tail);
    digits > 0 && tail.bytes().all(|b| b.is_ascii_digit())
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
    fn a_version_segment_the_base_url_ends_with_is_not_repeated() {
        // 服务商文档给的地址带着 `/v1`，客户端的路径也带着
        assert_eq!(
            upstream_url("https://api.openai.com/v1", "/v1/chat/completions", None),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            upstream_url("https://api.groq.com/openai/v1/", "/v1/models", None),
            "https://api.groq.com/openai/v1/models"
        );
        assert_eq!(
            upstream_url(
                "https://generativelanguage.googleapis.com/v1beta",
                "/v1beta/models/m:streamGenerateContent",
                Some("alt=sse")
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/m:streamGenerateContent?alt=sse"
        );
        // 不带版本段的地址照旧接在后面
        assert_eq!(
            upstream_url("https://relay.example/anthropic", "/v1/messages", None),
            "https://relay.example/anthropic/v1/messages"
        );
    }

    #[test]
    fn only_the_same_whole_version_segment_counts_as_a_repeat() {
        // 不同的版本段是两段
        assert_eq!(
            upstream_url("https://x.example/v1", "/v1beta/models/m", None),
            "https://x.example/v1/v1beta/models/m"
        );
        // 不是版本段的同名段不合并：说明不了是同一段
        assert_eq!(
            upstream_url("https://x.example/api", "/api/items", None),
            "https://x.example/api/api/items"
        );
        // 主机名不是路径段
        assert_eq!(
            upstream_url("http://v1:8080", "/v1/messages", None),
            "http://v1:8080/v1/messages"
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
