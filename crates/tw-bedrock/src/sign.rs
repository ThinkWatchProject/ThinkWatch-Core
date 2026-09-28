//! AWS SigV4 签名，以及什么时候不签。
//!
//! 不带 API Key 的 Bedrock 上游没有 bearer token 可发：每个请求按内容签名，签的是
//! 方法、URL、时间和请求体的哈希。所以**签名必须在请求体和 URL 都定稿之后**，
//! 改一个字节签名就对不上。
//!
//! 凭证从哪儿来不归这里管：配置里的访问密钥、EC2 实例角色，都由调用方取到之后交进来。

use std::fmt;
use std::time::SystemTime;

/// Bedrock 的签名服务名。推理（`bedrock-runtime`）和控制面（`bedrock`）都签成它。
const SERVICE: &str = "bedrock";

/// 签名用的一组 AWS 凭证。
///
/// **Debug 不打印密钥**：它会跟着调用方的结构进日志和 panic 信息。访问密钥 ID 只露
/// 前四个字符，`AKIA` 是长期密钥、`ASIA` 是临时凭证，查问题时要看的就是这个。
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    /// 临时凭证才有（STS 发的那种）
    pub session_token: Option<String>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let head: String = self.access_key_id.chars().take(4).collect();
        f.debug_struct("Credentials")
            .field("access_key_id", &format_args!("{head}…"))
            .field("secret_access_key", &Hidden)
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| Hidden),
            )
            .finish()
    }
}

/// Debug 里代替一个密钥
struct Hidden;

impl fmt::Debug for Hidden {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<hidden>")
    }
}

/// 签名没签成。
///
/// **这一层不认识调用方的错误体系** —— 企业版有它的网关错误，core 有自己的一套，
/// 共享层认识谁都不对。调用方在边界上换成自己的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignError(pub String);

impl fmt::Display for SignError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SigV4 signing failed: {}", self.0)
    }
}

impl std::error::Error for SignError {}

/// 上游自己的请求头里已经有凭证了吗 —— 有的话就不签。
///
/// Bedrock 认两种凭证。Bedrock API Key 是 bearer token，放在上游自己的
/// `Authorization` 头里：它就是全部凭证，没有什么可签的，而签了反而会多出第二个
/// `authorization` 头，AWS 会拒掉。没有它的时候每个请求都签 —— 发往控制面的
/// （列模型）也一样。头名不分大小写。
pub fn carries_api_key<'a>(header_names: impl IntoIterator<Item = &'a str>) -> bool {
    header_names
        .into_iter()
        .any(|n| n.eq_ignore_ascii_case("authorization"))
}

/// 签一个请求，返回要加上去的头：`authorization`、`x-amz-date`、
/// `x-amz-content-sha256`，临时凭证还有 `x-amz-security-token`。只有这几个 ——
/// 别的头是调用方的，签名不该替它改。
///
/// - `url` 必须是**实际发出去的那个**，查询串也算：签名覆盖方法和整个 URL。
/// - `body` 必须和发出去的一个字节都不差；它的 `content-type`
///   （`application/json`）也一起签。`None` 是没有请求体的请求，比如 GET：签的是
///   空内容的哈希，也没有 content-type 可签。
/// - `at` 是签名时间，一般就是现在。AWS 只认前后几分钟以内的签名。
pub fn sign(
    credentials: &Credentials,
    region: &str,
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    at: SystemTime,
) -> Result<Vec<(String, String)>, SignError> {
    use aws_sigv4::http_request::{
        PayloadChecksumKind, SignableBody, SignableRequest, SignatureLocation, SigningSettings,
        sign,
    };
    use aws_sigv4::sign::v4;

    let fail = |e: &dyn fmt::Display| SignError(e.to_string());
    let identity = aws_credential_types::Credentials::new(
        &credentials.access_key_id,
        &credentials.secret_access_key,
        credentials.session_token.clone(),
        None,
        "thinkwatch",
    )
    .into();

    let mut settings = SigningSettings::default();
    // 请求体的哈希放进 `x-amz-content-sha256` 头：签名覆盖它，AWS 拿它核对请求体
    settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
    settings.signature_location = SignatureLocation::Headers;

    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(SERVICE)
        .time(at)
        .settings(settings)
        .build()
        .map_err(|e| fail(&e))?;

    let content_type = body.map(|_| ("content-type", "application/json"));
    let signable = SignableRequest::new(
        method,
        url,
        content_type.into_iter(),
        SignableBody::Bytes(body.unwrap_or_default()),
    )
    .map_err(|e| fail(&e))?;

    let (instructions, _signature) = sign(signable, &params.into())
        .map_err(|e| fail(&e))?
        .into_parts();
    let (headers, _query) = instructions.into_parts();
    Ok(headers
        .into_iter()
        .map(|h| (h.name().to_ascii_lowercase(), h.value().to_string()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Credentials {
        Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        }
    }

    #[test]
    fn debug_never_prints_a_secret() {
        let c = Credentials {
            session_token: Some("FwoGZXIvYXdzEXAMPLETOKEN".into()),
            ..keys()
        };
        let shown = format!("{c:?}");
        assert!(shown.contains("AKIA…"), "{shown}");
        for secret in [
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI",
            "FwoGZXIvYXdzEXAMPLETOKEN",
        ] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }

    #[test]
    fn an_authorization_header_in_any_case_is_an_api_key() {
        assert!(carries_api_key(["x-custom", "Authorization"]));
        assert!(carries_api_key(["AUTHORIZATION"]));
        assert!(!carries_api_key(["x-custom", "x-api-key"]));
        assert!(!carries_api_key([]));
    }
}
