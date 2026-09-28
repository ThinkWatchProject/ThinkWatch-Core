//! Bedrock 上游用 AWS 的 profile：密钥从凭证文件读，**文件变了下一个请求就用新的**。
//!
//! 单独一个测试程序：凭证文件在哪儿由环境变量 `AWS_SHARED_CREDENTIALS_FILE` 说，而
//! 进程里改环境变量要保证没有别的线程同时在读 —— 和别的测试放在一起就保证不了。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::http::HeaderMap;
use serde_json::json;
use tw_config::{Aws, Client, Config, Listen, Protocol, Provider};

const MODEL: &str = "us.anthropic.claude-sonnet-4-5-20250929-v1:0";

/// 假的 Bedrock：记下每个请求的 `authorization`，回一段 Converse 的回答
async fn bedrock() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = seen.clone();
    let app = Router::new().fallback(move |headers: HeaderMap| {
        let log = log.clone();
        async move {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            log.lock().unwrap().push(auth);
            axum::Json(json!({
                "output": {"message": {"role": "assistant", "content": [{"text": "hi"}]}},
                "stopReason": "end_turn",
                "usage": {"inputTokens": 3, "outputTokens": 1, "totalTokens": 4}
            }))
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, seen)
}

fn credentials(key_id: &str) -> String {
    format!("[dev]\naws_access_key_id = {key_id}\naws_secret_access_key = secret-{key_id}\n")
}

#[tokio::test]
async fn a_profile_signs_with_what_its_file_says_now() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("credentials");
    std::fs::write(&file, credentials("AKIAPROFILEONE")).unwrap();
    // SAFETY: 这个测试程序里只有这一条测试，改的时候还没有别的线程在读环境变量
    unsafe {
        std::env::set_var("AWS_SHARED_CREDENTIALS_FILE", &file);
        std::env::set_var("AWS_CONFIG_FILE", dir.path().join("config"));
    }

    let (up, seen) = bedrock().await;
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "br".into(),
            base_url: format!("http://{up}"),
            protocol: Some(Protocol::Bedrock),
            aws: Some(Aws {
                profile: Some("dev".into()),
                region: Some("us-west-2".into()),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let gw = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ask = || async {
        reqwest::Client::new()
            .post(format!("http://{gw}/v1/chat/completions"))
            .bearer_auth("tw-k")
            .json(&json!({"model": MODEL, "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    };

    assert_eq!(ask().await, 200);
    // 刷新凭证的工具把新的密钥写回同一个文件
    std::fs::write(&file, credentials("AKIAPROFILETWO")).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(5))
        .unwrap();
    assert_eq!(ask().await, 200);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(
        seen[0].starts_with("AWS4-HMAC-SHA256 Credential=AKIAPROFILEONE/")
            && seen[0].contains("/us-west-2/bedrock/aws4_request"),
        "{}",
        seen[0]
    );
    assert!(
        seen[1].starts_with("AWS4-HMAC-SHA256 Credential=AKIAPROFILETWO/"),
        "the rewritten file was not read again: {}",
        seen[1]
    );
}
