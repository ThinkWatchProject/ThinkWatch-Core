//! 测试共用：照着 SigV4 规范另算一遍签名，和一个假的控制面。
//!
//! 签名的核对**不用签名库**：两边各写各的，才不会因为同一个错而对上。

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const AK: &str = "AKIAIOSFODNN7EXAMPLE";
pub const SK: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

pub fn keys() -> tw_bedrock::Credentials {
    tw_bedrock::Credentials {
        access_key_id: AK.into(),
        secret_access_key: SK.into(),
        session_token: None,
    }
}

/// 收到的一个请求。
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    /// 原样，没解码
    pub path: String,
    /// 原样，没解码，不含 `?`
    pub query: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Seen {
    /// 查询参数，解码之后
    pub fn params(&self) -> Vec<(String, String)> {
        reqwest::Url::parse(&format!("http://x/?{}", self.query))
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    pub fn param(&self, name: &str) -> Option<String> {
        self.params()
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn all(&self, name: &str) -> Vec<String> {
        self.headers
            .get_all(name)
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect()
    }
}

/// 一个假的控制面：每个请求交给 `answer`，记下收到了什么。
pub struct Fake {
    pub addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    pub async fn start(
        answer: impl Fn(&Seen) -> (StatusCode, Vec<(&'static str, String)>, Value)
        + Send
        + Sync
        + 'static,
    ) -> Fake {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let answer = Arc::new(answer);
        let app = Router::new().fallback(move |req: axum::extract::Request| {
            let log = log.clone();
            let answer = answer.clone();
            async move {
                let (parts, body) = req.into_parts();
                let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                let s = Seen {
                    method: parts.method.to_string(),
                    path: parts.uri.path().to_string(),
                    query: parts.uri.query().unwrap_or_default().to_string(),
                    headers: parts.headers,
                    body: body.to_vec(),
                };
                let (status, headers, json) = answer(&s);
                log.lock().unwrap().push(s);
                let mut resp =
                    axum::response::IntoResponse::into_response((status, axum::Json(json)));
                for (k, v) in headers {
                    resp.headers_mut().insert(k, v.parse().unwrap());
                }
                resp
            }
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        Fake { addr, seen }
    }

    pub fn uri(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC-SHA256 accepts any key length");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// RFC 3986 编码，SigV4 规范化时用的那一种：只留字母数字和 `-_.~`。
pub fn aws_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// 请求自称的签名，和 AWS 按收到的东西算出来的那个。
///
/// 规范化照 SigV4：路径按段再编码一次（S3 以外的服务都这样，所以线上的 `%2F`
/// 在规范请求里是 `%252F`），查询参数解码后重新编码、排序，签过的头按名字写出来，
/// 请求体的哈希取自 `x-amz-content-sha256`，并核对它就是收到的请求体的哈希。
pub fn signature_of(req: &Seen, secret: &str, region: &str) -> (String, String) {
    let header = |name: &str| {
        req.header(name)
            .unwrap_or_else(|| panic!("no {name} on {} {}", req.method, req.path))
            .to_string()
    };
    let auth = header("authorization");
    let field = |name: &str| {
        auth.split(&format!("{name}="))
            .nth(1)
            .and_then(|rest| rest.split(',').next())
            .unwrap_or_else(|| panic!("no {name} in {auth}"))
            .to_string()
    };
    let signed_headers = field("SignedHeaders");
    let amz_date = header("x-amz-date");
    let payload_hash = header("x-amz-content-sha256");
    assert_eq!(
        payload_hash,
        hex::encode(Sha256::digest(&req.body)),
        "the signed payload hash is not the hash of what arrived"
    );

    let path = req
        .path
        .split('/')
        .map(aws_encode)
        .collect::<Vec<_>>()
        .join("/");
    let mut query: Vec<(String, String)> = req
        .params()
        .iter()
        .map(|(k, v)| (aws_encode(k), aws_encode(v)))
        .collect();
    query.sort();
    let query = query
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let canonical_headers = signed_headers
        .split(';')
        .map(|name| format!("{name}:{}\n", header(name).trim()))
        .collect::<String>();
    let canonical_request = format!(
        "{}\n{path}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        req.method
    );

    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/bedrock/aws4_request");
    let credential = field("Credential");
    assert!(
        credential.ends_with(&format!("/{scope}")),
        "{credential} is not scoped to {scope}"
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );
    let key = hmac(format!("AWS4{secret}").as_bytes(), date);
    let key = hmac(&key, region);
    let key = hmac(&key, "bedrock");
    let key = hmac(&key, "aws4_request");
    (field("Signature"), hex::encode(hmac(&key, &string_to_sign)))
}
