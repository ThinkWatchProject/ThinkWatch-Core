//! SigV4：签出来的头，以及 AWS 按收到的请求算出来的签名对不对得上。

mod common;

use std::time::{Duration, SystemTime};

use axum::http::StatusCode;
use common::{AK, Fake, SK, keys, signature_of};
use serde_json::json;
use tw_bedrock::sign::sign;

const CONVERSE: &str = "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse";

fn now() -> SystemTime {
    SystemTime::now()
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> &'a str {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
        .unwrap_or_else(|| panic!("no {name} in {headers:?}"))
}

/// `authorization` 头里的 `SignedHeaders=`
fn signed_headers(headers: &[(String, String)]) -> &str {
    let auth = header(headers, "authorization");
    auth.split("SignedHeaders=")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .unwrap_or_else(|| panic!("no SignedHeaders in {auth}"))
}

#[test]
fn signing_produces_an_authorization_header_and_the_payload_hash() {
    let headers = sign(&keys(), "us-east-1", "POST", CONVERSE, Some(b"{}"), now()).unwrap();
    let names: Vec<&str> = headers.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"authorization"), "{names:?}");
    assert!(
        names.contains(&"x-amz-content-sha256"),
        "the signature covers the body hash, so this header must be there: {names:?}"
    );
    assert!(names.contains(&"x-amz-date"), "{names:?}");
    assert!(
        header(&headers, "authorization")
            .starts_with("AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/")
    );
}

#[test]
fn nothing_but_the_signed_headers_comes_back() {
    // 把别的头也带回去，会盖掉调用方自己设的
    let headers = sign(&keys(), "us-east-1", "POST", CONVERSE, Some(b"{}"), now()).unwrap();
    for (n, _) in &headers {
        assert!(
            n == "authorization" || n.starts_with("x-amz-"),
            "{n} was not produced by signing"
        );
    }
}

#[test]
fn a_different_body_signs_differently() {
    // 签名覆盖请求体：改一个字节就得签成不同的，否则改过请求体的重放照样能过
    let a = sign(&keys(), "us-east-1", "POST", CONVERSE, Some(b"{}"), now()).unwrap();
    let b = sign(
        &keys(),
        "us-east-1",
        "POST",
        CONVERSE,
        Some(b"{\"x\":1}"),
        now(),
    )
    .unwrap();
    assert_ne!(
        header(&a, "x-amz-content-sha256"),
        header(&b, "x-amz-content-sha256")
    );
}

#[test]
fn a_json_body_signs_its_content_type() {
    let headers = sign(&keys(), "us-east-1", "POST", CONVERSE, Some(b"{}"), now()).unwrap();
    assert_eq!(
        signed_headers(&headers),
        "content-type;host;x-amz-content-sha256;x-amz-date"
    );
}

#[test]
fn a_get_signs_an_empty_payload_and_no_content_type() {
    // GET 既没有请求体也没有 content-type：签了它们，签的就是一个不会发出去的请求
    let headers = sign(
        &keys(),
        "us-east-1",
        "GET",
        "https://bedrock.us-east-1.amazonaws.com/inference-profiles?type=SYSTEM_DEFINED",
        None,
        now(),
    )
    .unwrap();
    assert_eq!(
        header(&headers, "x-amz-content-sha256"),
        // 零字节的 SHA-256
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        signed_headers(&headers),
        "host;x-amz-content-sha256;x-amz-date"
    );
}

#[test]
fn temporary_credentials_send_and_sign_their_session_token() {
    let temporary = tw_bedrock::Credentials {
        session_token: Some("FwoGZXIvYXdzEXAMPLE".into()),
        ..keys()
    };
    let headers = sign(
        &temporary,
        "us-east-1",
        "POST",
        CONVERSE,
        Some(b"{}"),
        now(),
    )
    .unwrap();
    assert_eq!(
        header(&headers, "x-amz-security-token"),
        "FwoGZXIvYXdzEXAMPLE"
    );
    assert_eq!(
        signed_headers(&headers),
        "content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token"
    );
}

#[test]
fn the_same_request_at_the_same_moment_signs_the_same() {
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
    let a = sign(&keys(), "us-east-1", "POST", CONVERSE, Some(b"{}"), at).unwrap();
    let b = sign(&keys(), "us-east-1", "POST", CONVERSE, Some(b"{}"), at).unwrap();
    assert_eq!(a, b);
    assert_eq!(header(&a, "x-amz-date"), "20260921T141320Z");
}

/// 签过的请求真的发出去，按 AWS 的算法在收到的一端重算：路径里有转义过的 ARN、
/// 带请求体、带临时凭证。
#[tokio::test]
async fn aws_computes_the_same_signature_from_what_arrives() {
    let fake = Fake::start(|_| (StatusCode::OK, vec![], json!({}))).await;
    let path = "/model/arn:aws:bedrock:us-east-2:123456789012:application-inference-profile%2Fa1b2c3/converse";
    let url = format!("{}{path}", fake.uri());
    let body = br#"{"messages":[{"role":"user","content":[{"text":"hi"}]}]}"#;
    let temporary = tw_bedrock::Credentials {
        session_token: Some("FwoGZXIvYXdzEXAMPLE".into()),
        ..keys()
    };

    // 签的是发出去的那个 URL 字符串（reqwest 解析过的）
    let parsed = reqwest::Url::parse(&url).unwrap();
    let signed = sign(
        &temporary,
        "us-east-2",
        "POST",
        parsed.as_str(),
        Some(body),
        now(),
    )
    .unwrap();
    let mut req = reqwest::Client::new()
        .post(parsed)
        .header("content-type", "application/json");
    for (k, v) in signed {
        req = req.header(k, v);
    }
    let status = req.body(body.to_vec()).send().await.unwrap().status();
    assert_eq!(status, 200);

    let seen = fake.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, path, "the ARN's slash must arrive encoded");
    let (claimed, expected) = signature_of(&seen[0], SK, "us-east-2");
    assert_eq!(claimed, expected);
    assert!(seen[0].header("authorization").unwrap().contains(AK));
}
