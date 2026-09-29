//! Bedrock 上游在控制面上：按地址认出区域、访问密钥原样存取、凭证二选一、
//! 检查连接走区域的控制面。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

const BASE: &str = "version: 1
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
clients:
  - name: c
    key: tw-k
providers:
  - name: 官方
    base_url: https://api.anthropic.com
    key: sk-official
";

struct Bed {
    dir: tempfile::TempDir,
    app: axum::Router,
}

impl Bed {
    fn file(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("config.yaml")).unwrap()
    }
}

fn bed(yaml: &str) -> Bed {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, yaml).unwrap();
    let cfg = tw_config::try_parse(yaml).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let state = ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        store: None,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    };
    Bed {
        app: tw_control::router(state),
        dir: d,
    }
}

async fn call(app: &axum::Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let b = axum::body::to_bytes(r.into_body(), 1 << 22).await.unwrap();
    (status, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_bedrock_address_is_previewed_with_its_protocol_and_region() {
    let b = bed(BASE);
    let (st, v) = call(
        &b.app,
        "POST",
        "/providers/preview",
        json!({"base_url": "https://bedrock-runtime.eu-west-3.amazonaws.com"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["protocol"], "bedrock");
    assert_eq!(v["auth_header"], "authorization");
    assert_eq!(v["region"], "eu-west-3");

    let (_, v) = call(
        &b.app,
        "POST",
        "/providers/preview",
        json!({"base_url": "https://api.anthropic.com"}),
    )
    .await;
    assert!(v.get("region").is_none(), "{v}");
}

#[tokio::test]
async fn access_keys_are_saved_and_shown_as_written() {
    let b = bed(BASE);
    let (st, v) = call(
        &b.app,
        "POST",
        "/providers",
        json!({"provider": {
            "name": "bedrock",
            "base_url": "https://bedrock-runtime.us-east-1.amazonaws.com",
            "aws": {
                "access_key_id": "${AWS_ACCESS_KEY_ID}",
                "secret_access_key": "${AWS_SECRET_ACCESS_KEY}",
                // 界面交上来的空框：就是没有
                "session_token": "  ",
            },
        }}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let file = b.file();
    assert!(
        file.contains("access_key_id: ${AWS_ACCESS_KEY_ID}"),
        "{file}"
    );
    assert!(!file.contains("session_token"), "{file}");

    let (_, v) = call(&b.app, "GET", "/overview", Value::Null).await;
    let p = v["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "bedrock")
        .unwrap()
        .clone();
    assert_eq!(p["protocol"], "bedrock");
    assert_eq!(p["region"], "us-east-1");
    assert_eq!(p["aws"]["access_key_id"], "${AWS_ACCESS_KEY_ID}");
    assert_eq!(p["aws"]["secret_access_key"], "${AWS_SECRET_ACCESS_KEY}");
    assert!(p["aws"].get("session_token").is_none(), "{p}");
    assert!(p.get("key").is_none(), "{p}");
}

/// profile 只存名字：密钥留在 AWS 的凭证文件里，**配置里一个字都不抄**
#[tokio::test]
async fn a_profile_is_saved_by_name_alone() {
    let b = bed(BASE);
    let (st, v) = call(
        &b.app,
        "POST",
        "/providers",
        json!({"provider": {
            "name": "bedrock",
            "base_url": "https://bedrock-runtime.us-west-2.amazonaws.com",
            "aws": {"profile": " dev ", "access_key_id": "", "secret_access_key": ""},
        }}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let file = b.file();
    assert!(file.contains("profile: dev"), "{file}");
    assert!(!file.contains("access_key_id"), "{file}");

    let (_, v) = call(&b.app, "GET", "/overview", Value::Null).await;
    let p = v["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "bedrock")
        .unwrap()
        .clone();
    assert_eq!(p["aws"], json!({"profile": "dev"}), "{p}");
    assert_eq!(p["region"], "us-west-2");
}

#[tokio::test]
async fn a_bedrock_upstream_is_refused_without_exactly_one_credential() {
    let b = bed(BASE);
    let url = "https://bedrock-runtime.us-east-1.amazonaws.com";
    let keys = json!({"access_key_id": "AKIAIOSFODNN7EXAMPLE", "secret_access_key": "s"});
    for (provider, code) in [
        (
            json!({"name": "b", "base_url": url, "key": "ABSK-x", "aws": keys}),
            "config.credential.key_and_aws",
        ),
        (
            json!({"name": "b", "base_url": url}),
            "config.credential.bedrock_no_credential",
        ),
        (
            json!({"name": "b", "base_url": "https://api.openai.com", "aws": keys}),
            "config.credential.aws_not_bedrock",
        ),
        (
            json!({"name": "b", "base_url": url, "aws": {"access_key_id": "AKIA", "secret_access_key": "s", "region": "eu-west-1"}}),
            "config.credential.aws_region_mismatch",
        ),
        (
            json!({"name": "b", "base_url": url, "aws": {"access_key_id": "AKIA", "secret_access_key": "s", "profile": "dev"}}),
            "config.credential.aws_profile_and_keys",
        ),
        (
            // 界面的空框什么都不算：既没有密钥，也没有 profile
            json!({"name": "b", "base_url": url, "aws": {"access_key_id": " ", "profile": ""}}),
            "config.credential.aws_empty",
        ),
    ] {
        let (st, v) = call(&b.app, "POST", "/providers", json!({"provider": provider})).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["code"], code, "{v}");
    }
}

/// 一个假的控制面：`answer` 决定怎么回
async fn control_plane(
    answer: impl Fn(&str) -> (u16, Option<&'static str>, Value) + Clone + Send + Sync + 'static,
) -> std::net::SocketAddr {
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
        let answer = answer.clone();
        async move {
            let (status, kind, body) = answer(uri.path());
            let mut r = axum::response::Response::builder()
                .status(status)
                .header("content-type", "application/json");
            if let Some(k) = kind {
                r = r.header("x-amzn-errortype", k);
            }
            r.body(Body::from(body.to_string())).unwrap()
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

async fn test_with(b: &Bed, up: std::net::SocketAddr) -> Value {
    let (st, v) = call(
        &b.app,
        "POST",
        "/providers/test",
        json!({"provider": {
            "name": "b",
            "base_url": format!("http://{up}"),
            "protocol": "bedrock",
            "key": "ABSK-x",
        }}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v
}

#[tokio::test]
async fn testing_a_bedrock_upstream_lists_what_it_can_route_to() {
    let up = control_plane(|path| match path {
        "/foundation-models" => (
            200,
            None,
            json!({"modelSummaries": [{"modelId": "amazon.nova-lite-v1:0"}]}),
        ),
        _ => (
            200,
            None,
            json!({"inferenceProfileSummaries": [{"inferenceProfileId": "us.anthropic.claude-sonnet-4-5-20250929-v1:0", "inferenceProfileArn": "arn:x"}]}),
        ),
    })
    .await;
    let b = bed(BASE);
    let v = test_with(&b, up).await;
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["protocol"], "bedrock");
    assert_eq!(v["models"]["kind"], "listed");
    let models = v["models"]["models"].as_array().unwrap();
    assert!(models.iter().any(|m| m == "amazon.nova-lite-v1:0"), "{v}");
}

#[tokio::test]
async fn no_permission_to_list_is_not_a_rejected_key() {
    // AWS 认出了身份，只是不给列：凭证能用
    let denied = control_plane(|_| {
        (
            403,
            Some("AccessDeniedException:http://internal.amazon.com/coral/com.amazon.bedrock/"),
            json!({"message": "User: arn:aws:iam::123456789012:user/alice is not authorized to perform: bedrock:ListFoundationModels because no identity-based policy allows the bedrock:ListFoundationModels action"}),
        )
    })
    .await;
    let b = bed(BASE);
    let v = test_with(&b, denied).await;
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["models"]["kind"], "not_implemented", "{v}");
    assert_eq!(v["error"]["code"], "gw.probe.bedrock_list_denied", "{v}");
    assert!(!v.to_string().contains("123456789012"), "{v}");

    let rejected = control_plane(|_| {
        (
            403,
            Some("UnrecognizedClientException"),
            json!({"message": "The security token included in the request is invalid."}),
        )
    })
    .await;
    let v = test_with(&b, rejected).await;
    assert_eq!(v["ok"], false, "{v}");
    assert_eq!(v["error"]["code"], "gw.probe.key_rejected", "{v}");
}

/// Bedrock API Key 本身不对时，AWS 回的**也是** `AccessDeniedException`，只是原话不同（这两句
/// 是拿格式不对、和格式对但不存在的 key 实际问出来的）。那是密钥被拒，不能说成「凭证能用、
/// 只是列不了」—— 那样填错了 key，「检查连接」反倒说没问题
#[tokio::test]
async fn a_bedrock_api_key_that_is_not_valid_is_rejected() {
    for message in [
        "Authentication failed: Please make sure your API Key is valid.",
        "Invalid API Key format: Must start with pre-defined prefix",
    ] {
        let up = control_plane(move |_| {
            (
                403,
                Some("AccessDeniedException:http://internal.amazon.com/coral/com.amazon.coral.service/"),
                json!({"Message": message}),
            )
        })
        .await;
        let b = bed(BASE);
        let v = test_with(&b, up).await;
        assert_eq!(v["ok"], false, "{message}: {v}");
        assert_eq!(
            v["error"]["code"], "gw.probe.key_rejected",
            "{message}: {v}"
        );
    }
}
