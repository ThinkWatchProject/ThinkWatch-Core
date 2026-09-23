//! AWS SigV4 签名。
//!
//! Bedrock 是唯一一家不用 bearer token 的上游:每个请求按内容签名，签的是
//! 方法、URL、时间和请求体的哈希。所以**签名必须在请求体最终定稿之后**，
//! 改一个字节签名就失效。
//!
//! 桌面版不接 Bedrock，所以这一整套在 `bedrock` feature 后面 —— 否则它会把
//! `aws-sigv4`、`aws-credential-types`、`aws-smithy-*` 连同一个 `http 0.2`
//! 拖给每一个依赖这个 crate 的人。

use std::time::SystemTime;

use aws_credential_types::Credentials;

/// 签名过程里出的错。
///
/// **这一层不认识调用方的错误体系** —— 企业版有 `GatewayError`，桌面版有
/// 自己的一套，共享层认识谁都是错的。调用方在边界上 `From` 一下。
#[derive(Debug)]
pub enum SignError {
    /// 拿不到凭据:环境里没有，IMDSv2 也问不出来
    Credentials(String),
    /// 签名本身失败
    Signing(String),
}

impl std::fmt::Display for SignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignError::Credentials(m) => write!(f, "AWS credentials are unavailable: {m}"),
            SignError::Signing(m) => write!(f, "SigV4 signing failed: {m}"),
        }
    }
}

impl std::error::Error for SignError {}

/// 一家 Bedrock 上游的签名身份。
///
/// 不带密钥时从 EC2 实例元数据（IMDSv2）现取 —— 那是在 AWS 里跑的部署
/// 该有的样子:凭据由实例角色给，会自动轮换，不落在配置文件里。
pub struct Signer {
    pub region: String,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
}

/// IMDS 的地址。**是个链路本地地址**，只有在 EC2 里才答得上来。
const IMDS: &str = "http://169.254.169.254";

impl Signer {
    /// 签一个请求，返回要加上去的头（`authorization` 和 `x-amz-*`）。
    ///
    /// `body` 必须是**最终要发出去的那一份**:签名覆盖请求体的哈希。
    pub async fn sign(
        &self,
        client: &reqwest::Client,
        url: &str,
        body: &[u8],
    ) -> Result<Vec<(String, String)>, SignError> {
        use aws_sigv4::http_request::{
            PayloadChecksumKind, SignableBody, SignableRequest, SignatureLocation, SigningSettings,
            sign,
        };
        use aws_sigv4::sign::v4;

        let credentials = match (&self.access_key_id, &self.secret_access_key) {
            (Some(ak), Some(sk)) => Credentials::new(ak, sk, None, None, "think-watch"),
            _ => self.imdsv2_credentials(client).await?,
        };

        let identity = credentials.into();
        let mut settings = SigningSettings::default();
        settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
        settings.signature_location = SignatureLocation::Headers;

        let params = v4::SigningParams::builder()
            .identity(&identity)
            .region(&self.region)
            .name("bedrock")
            .time(SystemTime::now())
            .settings(settings)
            .build()
            .map_err(|e| SignError::Signing(e.to_string()))?;

        let signable = SignableRequest::new(
            "POST",
            url,
            std::iter::once(("content-type", "application/json")),
            SignableBody::Bytes(body),
        )
        .map_err(|e| SignError::Signing(e.to_string()))?;

        let (instructions, _signature) = sign(signable, &params.into())
            .map_err(|e| SignError::Signing(e.to_string()))?
            .into_parts();

        // 签名库只会往一个 http 请求上写，所以造一个空的接住它
        let mut req = http::Request::builder()
            .method("POST")
            .uri(url)
            .header("content-type", "application/json")
            .body(())
            .map_err(|e| SignError::Signing(e.to_string()))?;
        instructions.apply_to_request_http1x(&mut req);

        // 只要签出来的那几个。**别的头是调用方的** —— 把整份头带走会
        // 盖掉它自己设的东西
        Ok(req
            .headers()
            .iter()
            .filter(|(n, _)| {
                let n = n.as_str();
                n == "authorization" || n.starts_with("x-amz-")
            })
            .map(|(n, v)| (n.to_string(), v.to_str().unwrap_or_default().to_string()))
            .collect())
    }

    /// 从 EC2 实例元数据取临时凭据。
    ///
    /// IMDSv2 是三步:先拿一个有生存期的令牌，再问角色名，再用角色名换凭据。
    /// v1 一步就能问出来，也正因为如此，一个能发出 SSRF 的应用就能把凭据
    /// 读走 —— v2 的令牌要用 PUT 取，而 SSRF 通常只能发 GET。
    async fn imdsv2_credentials(&self, client: &reqwest::Client) -> Result<Credentials, SignError> {
        let fail = |what: &str, e: reqwest::Error| SignError::Credentials(format!("{what}: {e}"));

        let token = client
            .put(format!("{IMDS}/latest/api/token"))
            .header("X-aws-ec2-metadata-token-ttl-seconds", "300")
            .send()
            .await
            .map_err(|e| fail("IMDSv2 token request", e))?
            .text()
            .await
            .map_err(|e| fail("IMDSv2 token read", e))?;

        let role = client
            .get(format!("{IMDS}/latest/meta-data/iam/security-credentials/"))
            .header("X-aws-ec2-metadata-token", &token)
            .send()
            .await
            .map_err(|e| fail("IMDSv2 role lookup", e))?
            .text()
            .await
            .map_err(|e| fail("IMDSv2 role read", e))?;
        let role = role.trim();

        let creds: serde_json::Value = client
            .get(format!(
                "{IMDS}/latest/meta-data/iam/security-credentials/{role}"
            ))
            .header("X-aws-ec2-metadata-token", &token)
            .send()
            .await
            .map_err(|e| fail("IMDSv2 credentials fetch", e))?
            .json()
            .await
            .map_err(|e| fail("IMDSv2 credentials parse", e))?;

        let field = |k: &str| {
            creds[k]
                .as_str()
                .ok_or_else(|| SignError::Credentials(format!("IMDSv2 response has no {k}")))
        };
        Ok(Credentials::new(
            field("AccessKeyId")?,
            field("SecretAccessKey")?,
            creds["Token"].as_str().map(str::to_string),
            None,
            "imdsv2",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer() -> Signer {
        Signer {
            region: "us-east-1".into(),
            access_key_id: Some("AKIAIOSFODNN7EXAMPLE".into()),
            secret_access_key: Some("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into()),
        }
    }

    #[tokio::test]
    async fn signing_produces_an_authorization_header_and_the_payload_hash() {
        let headers = signer()
            .sign(
                &reqwest::Client::new(),
                "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse",
                b"{}",
            )
            .await
            .expect("凭据是齐的，不该去问 IMDS");

        let names: Vec<&str> = headers.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"authorization"), "{names:?}");
        assert!(
            names.contains(&"x-amz-content-sha256"),
            "签名覆盖请求体的哈希，这个头必须在：{names:?}"
        );
        assert!(names.contains(&"x-amz-date"), "{names:?}");
    }

    #[tokio::test]
    async fn nothing_but_the_signed_headers_comes_back() {
        // 把整份头带走会盖掉调用方自己设的东西
        let headers = signer()
            .sign(
                &reqwest::Client::new(),
                "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse",
                b"{}",
            )
            .await
            .unwrap();
        for (n, _) in &headers {
            assert!(
                n == "authorization" || n.starts_with("x-amz-"),
                "{n} 不是签出来的"
            );
        }
    }

    #[tokio::test]
    async fn a_different_body_signs_differently() {
        // 签名覆盖请求体 —— 改一个字节就该是另一个签名。
        // 这条不成立的话，重放一个改过的请求体就能通过
        let c = reqwest::Client::new();
        let url = "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse";
        let a = signer().sign(&c, url, b"{}").await.unwrap();
        let b = signer().sign(&c, url, b"{\"x\":1}").await.unwrap();

        let hash = |h: &[(String, String)]| {
            h.iter()
                .find(|(n, _)| n == "x-amz-content-sha256")
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_ne!(hash(&a), hash(&b));
    }
}
