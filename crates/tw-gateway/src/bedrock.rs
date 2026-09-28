//! Bedrock 在网关里几处共用的一步：给一个定稿的请求签名。
//!
//! 转发、测速、重放都往 Bedrock 发请求，签名都要在**地址、请求头、请求体全部定稿之后**
//! 做，签的是 reqwest 实际要发的那个地址 —— 写三份的话，迟早有一份签的和发的不一样，
//! 而那时的症状只是上游一句 `SignatureDoesNotMatch`。

use std::time::SystemTime;

/// 用 `credentials` 给 `request` 签名，签出来的头写进去（覆盖同名的）。
///
/// 请求体必须是定长的（转发、测速、重放发的都是）：签名覆盖它的哈希。
pub fn sign_request(
    request: &mut reqwest::Request,
    credentials: &tw_bedrock::Credentials,
    region: &str,
) -> Result<(), tw_bedrock::SignError> {
    let body = request
        .body()
        .and_then(reqwest::Body::as_bytes)
        .unwrap_or_default()
        .to_vec();
    let headers = tw_bedrock::sign::sign(
        credentials,
        region,
        request.method().as_str(),
        request.url().as_str(),
        Some(&body),
        SystemTime::now(),
    )?;
    for (name, value) in headers {
        let (Ok(n), Ok(v)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(&value),
        ) else {
            return Err(tw_bedrock::SignError(format!(
                "the signature produced an unusable {name} header"
            )));
        };
        request.headers_mut().insert(n, v);
    }
    Ok(())
}
