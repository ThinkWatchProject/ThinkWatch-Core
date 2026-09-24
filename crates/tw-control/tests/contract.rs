//! tw-api 的端点表和真正挂上去的路由是一回事，失败一律是 [`tw_api::ErrorBody`]。
//!
//! 类型对不对在编译时就核过了（`contract.rs` 里的 `Serves`）。这里核的是编译器
//! 看不见的两件：**表里的每个端点都真的挂上了**，以及**每一种失败都能按
//! `ErrorBody` 解析** —— 包括 axum 在走到处理函数之前就回掉的那些。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_api::{Endpoint, ErrorBody, Method, ep};
use tw_control::{ConfigManager, ControlState};

const BASE: &str = "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: me\n    key: tw-contract-test-key\nproviders:\n  - name: official\n    base_url: https://api.anthropic.com\n    key: sk-x\n";

fn app() -> (tempfile::TempDir, axum::Router) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, BASE).unwrap();
    let cfg: tw_config::Config = serde_yaml_ng::from_str(BASE).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let state = ControlState {
        shutdown: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        store: None,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
        // **测试里绝不能碰开发者自己的配置**
        home: d.path().join("home"),
    };
    (d, tw_control::router(state))
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<&str>,
) -> (StatusCode, Option<String>) {
    let mut req = Request::builder().method(method).uri(uri);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let r = app
        .clone()
        .oneshot(
            req.body(Body::from(body.unwrap_or("").to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let st = r.status();
    // 事件流不会自己结束，不读它的响应体
    if st.is_success() {
        return (st, None);
    }
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (st, Some(String::from_utf8_lossy(&b).to_string()))
}

fn error(body: &str) -> ErrorBody {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("not an ErrorBody ({e}): {body}"))
}

/// 表里的每个端点都挂上了，方法也对。
///
/// **不让处理函数真的跑起来**（有的会去联网、有的会让进程退出）：
///
/// - 带请求体的端点发一个不是 JSON 的请求体。挂上了的话，axum 在走到处理函数
///   之前就拒掉它（400）；没挂上是 404 或 405。顺带核了「请求走请求体」这一条。
/// - 没有请求体的 POST 换一个它不收的方法去问：路径在，就是 405；不在是 404。
///   这几个端点的路径上只有它自己，所以 405 就说明它挂上了。
/// - GET 和 DELETE 真的发 —— 它们只读，或者删一个不存在的名字。
#[tokio::test]
async fn every_endpoint_in_the_table_is_served() {
    let (_d, app) = app();
    for e in ep::ALL {
        let path = tw_api::fill(
            e.path,
            &e.params.iter().map(|p| (*p, "1")).collect::<Vec<_>>(),
        );
        let what = format!("{} {} ({})", e.method.as_str(), e.path, e.name);
        match e.method {
            Method::Post | Method::Put | Method::Patch if e.has_req() => {
                let (st, body) = send(&app, e.method.as_str(), &path, Some("not json")).await;
                assert_eq!(st, StatusCode::BAD_REQUEST, "{what}: {body:?}");
                assert_eq!(
                    error(&body.unwrap()).code,
                    "control.request_rejected",
                    "{what}"
                );
            }
            Method::Post | Method::Put | Method::Patch => {
                let (st, body) = send(&app, "TRACE", &path, None).await;
                assert_eq!(st, StatusCode::METHOD_NOT_ALLOWED, "{what}: {body:?}");
                assert_eq!(
                    error(&body.unwrap()).code,
                    "control.method_not_allowed",
                    "{what}"
                );
            }
            Method::Get | Method::Delete => {
                let (st, body) = send(&app, e.method.as_str(), &path, None).await;
                assert_ne!(st, StatusCode::METHOD_NOT_ALLOWED, "{what}");
                if let Some(body) = body {
                    assert_ne!(error(&body).code, "control.no_such_endpoint", "{what}");
                }
            }
        }
    }
}

/// 框架替我们回的失败也是 `ErrorBody`，带着各自的码。
#[tokio::test]
async fn failures_the_framework_answers_are_messages_too() {
    let (_d, app) = app();

    let (st, body) = send(&app, "GET", "/no/such/thing", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let m = error(&body.unwrap());
    assert_eq!(m.code, "control.no_such_endpoint");
    assert_eq!(m.arg("path"), "/no/such/thing");

    let (st, body) = send(&app, "DELETE", ep::Status::PATH, None).await;
    assert_eq!(st, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(error(&body.unwrap()).code, "control.method_not_allowed");

    // 请求体读不成请求类型
    let (st, body) = send(
        &app,
        "POST",
        ep::ConfigRollback::PATH,
        Some("{\"version\":5}"),
    )
    .await;
    assert!(st.is_client_error(), "{st}");
    let m = error(&body.unwrap());
    assert_eq!(m.code, "control.request_rejected");
    assert!(!m.arg("detail").is_empty(), "{m:?}");

    // 路径参数读不成数字
    let (st, body) = send(&app, "GET", &ep::RequestDetail::path("abc"), None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(error(&body.unwrap()).code, "control.request_rejected");

    // 处理函数自己的失败照旧是它自己的码
    let (st, body) = send(&app, "GET", &ep::ProviderModels::path("nobody"), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(error(&body.unwrap()).code, "control.upstream_not_found");
}

/// 客户端按 tw-api 的类型拼查询串，core 按同一个类型读回来。
#[tokio::test]
async fn query_strings_built_from_the_contract_types_are_accepted() {
    let (_d, app) = app();
    let q = tw_api::GroupQuery {
        from_ms: Some(1),
        to_ms: Some(2),
        dim: tw_api::CostDim::Model,
    };
    let uri = format!(
        "{}?{}",
        ep::CostBy::PATH,
        serde_urlencoded::to_string(&q).unwrap()
    );
    let (st, body) = send(&app, "GET", &uri, None).await;
    // 没有请求库，处理函数说的是「记录读不了」，不是查询串读不懂
    let code = body.map(|b| error(&b).code);
    assert_ne!(code.as_deref(), Some("control.request_rejected"), "{st}");
}

/// 扫描可以带好几个项目目录。
#[tokio::test]
async fn scan_takes_more_than_one_project() {
    let (d, app) = app();
    let dirs: Vec<String> = ["a", "b"]
        .iter()
        .map(|n| {
            let p = d.path().join(n);
            std::fs::create_dir_all(&p).unwrap();
            p.display().to_string()
        })
        .collect();
    let body = serde_json::to_string(&tw_api::ScanRequest {
        projects: dirs.clone(),
    })
    .unwrap();
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method(ep::Scan::METHOD.as_str())
                .uri(ep::Scan::PATH)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    let got: tw_api::ScanResponse = serde_json::from_slice(&b).unwrap();
    assert_eq!(got.projects, dirs);
}
