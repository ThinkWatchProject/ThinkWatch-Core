//! AWS 的错误里能拿出来什么，哪些能给谁看。
//!
//! 出错时 AWS 在 `x-amzn-ErrorType` 头里给异常名，比如
//! `AccessDeniedException:http://internal.amazon.com/coral/com.amazon.bedrock/`；
//! 正文是 `{"message": "…"}`，有的错误写成 `Message`。
//!
//! **异常名能给人看，正文不一定。**401/403 的正文会点名账号 ID 和 IAM 身份
//! （`User: arn:aws:iam::123456789012:user/x is not authorized to perform …`），
//! 那不该出现在返回给客户端的文字里，也不该进日志里的错误类型。

use serde_json::Value;

/// AWS 放异常名的响应头。
pub const ERROR_TYPE: &str = "x-amzn-errortype";

/// 异常名：先看 `x-amzn-ErrorType` 头（冒号后面是命名空间，不要），没有再看正文的
/// `__type`（`com.amazon.coral.service#UnrecognizedClientException` 取 `#` 后面）。
///
/// 只认像名字的东西 —— 字母和数字。它会原样进错误信息，不能夹带别的。
pub fn kind_of(header: Option<&str>, body: &[u8]) -> Option<String> {
    let from_header = header.and_then(|h| h.split(':').next()).map(str::trim);
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let from_body = parsed
        .as_ref()
        .and_then(|v| v.get("__type"))
        .and_then(Value::as_str)
        .map(|t| t.rsplit('#').next().unwrap_or(t).trim());
    from_header
        .filter(|k| !k.is_empty())
        .or(from_body)
        .filter(|k| !k.is_empty() && k.len() <= 128 && k.bytes().all(|b| b.is_ascii_alphanumeric()))
        .map(str::to_string)
}

/// 正文里 AWS 说的原因：`message`，或者 `Message`。
pub fn message_in(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    v.get("message")
        .or_else(|| v.get("Message"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 一个异常名对应的 HTTP 状态。
///
/// 流里的异常事件来的时候 200 早已发出去了，异常名是剩下的唯一线索；状态照 AWS
/// 给同名异常定的那个，客户端和日志靠它分清「限流了，等等再试」和「请求本身不对」。
/// 大小写、有没有 `Exception` 结尾都认：流里是 `throttlingException`，HTTP 错误里是
/// `ThrottlingException`。认不出的算 502：上游出了错，说不清是哪种。
pub fn status_of(kind: &str) -> u16 {
    let k = kind.to_ascii_lowercase();
    let k = k.strip_suffix("exception").unwrap_or(&k);
    match k {
        "validation" | "servicequotaexceeded" => 400,
        "accessdenied" => 403,
        "resourcenotfound" => 404,
        "modeltimeout" => 408,
        "modelerror" | "modelstreamerror" => 424,
        "throttling" | "modelnotready" => 429,
        "internalserver" => 500,
        "serviceunavailable" => 503,
        _ => 502,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kind_comes_from_the_header_without_its_namespace() {
        assert_eq!(
            kind_of(
                Some("AccessDeniedException:http://internal.amazon.com/coral/com.amazon.bedrock/"),
                br#"{"message":"User: arn:aws:iam::123456789012:user/x is not authorized"}"#,
            )
            .as_deref(),
            Some("AccessDeniedException")
        );
    }

    #[test]
    fn without_the_header_the_kind_comes_from_the_body() {
        assert_eq!(
            kind_of(
                None,
                br#"{"__type":"com.amazon.coral.service#UnrecognizedClientException","message":"The security token included in the request is invalid."}"#,
            )
            .as_deref(),
            Some("UnrecognizedClientException")
        );
        assert_eq!(kind_of(None, b"not json"), None);
    }

    #[test]
    fn a_kind_that_is_not_a_name_is_not_repeated() {
        assert_eq!(kind_of(Some("Access Denied <script>"), b""), None);
    }

    #[test]
    fn the_message_is_read_in_either_spelling() {
        assert_eq!(message_in(br#"{"message":"a"}"#).as_deref(), Some("a"));
        assert_eq!(message_in(br#"{"Message":"b"}"#).as_deref(), Some("b"));
        assert_eq!(message_in(br#"{"error":"c"}"#), None);
        assert_eq!(message_in(b""), None);
    }

    #[test]
    fn a_stream_exception_takes_the_status_of_the_same_error_over_http() {
        assert_eq!(status_of("throttlingException"), 429);
        assert_eq!(status_of("ThrottlingException"), 429);
        assert_eq!(status_of("validationException"), 400);
        assert_eq!(status_of("serviceUnavailableException"), 503);
        assert_eq!(status_of("internalServerException"), 500);
        assert_eq!(status_of("modelStreamErrorException"), 424);
        assert_eq!(status_of("somethingNew"), 502);
    }
}
