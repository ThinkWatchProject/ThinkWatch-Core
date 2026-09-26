//! GLM Coding Plan 的额度：去额度接口问，和从 429 里认出额度用完。
//!
//! **全程只对本机的假 GLM**：模型接口和额度接口都是假的，key 也是编的。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::{any, get};
use tw_config::{Client, Config, Provider};

/// 假 GLM：模型接口回什么、额度接口回什么，以及额度接口收到的 `Authorization`
#[derive(Default)]
struct Glm {
    /// 模型接口的回答：(状态码, body)
    model: Mutex<(u16, String)>,
    /// 额度接口的 body
    quota: Mutex<String>,
    /// 额度接口每次收到的 `Authorization`
    asked: Mutex<Vec<String>>,
}

async fn model(State(g): State<Arc<Glm>>) -> axum::response::Response {
    let (status, body) = g.model.lock().unwrap().clone();
    (
        axum::http::StatusCode::from_u16(status).unwrap(),
        [("content-type", "application/json")],
        body,
    )
        .into_response()
}

async fn quota(State(g): State<Arc<Glm>>, headers: HeaderMap) -> axum::response::Response {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    g.asked.lock().unwrap().push(auth);
    let body = g.quota.lock().unwrap().clone();
    ([("content-type", "application/json")], body).into_response()
}

async fn start_glm(g: Arc<Glm>) -> SocketAddr {
    let app = Router::new()
        .route("/api/monitor/usage/quota/limit", get(quota))
        .fallback(any(model))
        .with_state(g);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

/// 一个指向假 GLM 的上游，额度接口也换成假 GLM 的
async fn state_for(glm: SocketAddr) -> tw_gateway::AppState {
    let state = tw_gateway::AppState::new(Config {
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "glm".into(),
            base_url: format!("http://{glm}/api/anthropic"),
            key: Some("fake-glm-key".into()),
            protocol: Some(tw_config::Protocol::Anthropic),
            ..Default::default()
        }],
        ..Default::default()
    })
    .unwrap();
    state.set_glm_sites(tw_gateway::glm::Sites {
        zai: format!("http://{glm}"),
        bigmodel: "https://open.bigmodel.cn".into(),
    });
    state
}

fn credit_plan(now_ms: u64) -> String {
    format!(
        r#"{{"code":200,"msg":"Operation successful","data":{{"limits":[{{"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":2000,"currentValue":23,"remaining":1976,"percentage":1,"nextResetTime":{five}}},{{"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":10000,"currentValue":268,"remaining":9731,"percentage":2,"nextResetTime":{week}}}],"level":"lite"}},"success":true}}"#,
        five = now_ms + 3_600_000,
        week = now_ms + 3 * 86_400_000,
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[tokio::test]
async fn asking_for_the_quota_reads_the_windows_and_credits() {
    let g = Arc::new(Glm::default());
    *g.quota.lock().unwrap() = credit_plan(now_ms());
    let state = state_for(start_glm(g.clone()).await).await;

    state.refresh_glm_quotas(Duration::from_secs(5)).await;
    let q = &state.quotas()["glm"];
    let names: Vec<_> = q.windows.iter().map(|w| w.window.as_str()).collect();
    assert_eq!(names, ["5h", "weekly"]);
    assert_eq!(
        q.windows[0].credits,
        Some(tw_api::QuotaCredits {
            total: 2000.0,
            used: 23.0,
            remaining: 1976.0
        })
    );
    // **key 原样放在 Authorization 里，不加 Bearer**
    assert_eq!(*g.asked.lock().unwrap(), ["fake-glm-key"]);

    // 一分钟之内再来要：不再问
    state.refresh_glm_quotas(Duration::from_secs(5)).await;
    assert_eq!(g.asked.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_key_without_a_plan_has_no_quota_and_is_not_asked_again_soon() {
    let g = Arc::new(Glm::default());
    *g.quota.lock().unwrap() =
        r#"{"code":500,"msg":"当前用户不存在coding plan","success":false}"#.into();
    let state = state_for(start_glm(g.clone()).await).await;

    state.refresh_glm_quotas(Duration::from_secs(5)).await;
    assert!(state.quotas().is_empty(), "没有套餐不是「用了 0%」");
    state.refresh_glm_quotas(Duration::from_secs(5)).await;
    assert_eq!(g.asked.lock().unwrap().len(), 1);
}

/// 判定没有套餐、清掉了记着的额度：**当场报一条窗口为空的 `QuotaSeen`**，界面不用等
/// 下一次读 `/quota` 才收起那一格。这里是 key 换成了一把没开通套餐的
#[tokio::test]
async fn a_key_found_to_have_no_plan_takes_its_quota_down_at_once() {
    let g = Arc::new(Glm::default());
    *g.quota.lock().unwrap() = credit_plan(now_ms());
    let state = state_for(start_glm(g.clone()).await).await;
    state.refresh_glm_quotas(Duration::from_secs(5)).await;
    assert_eq!(state.quotas()["glm"].windows.len(), 2);

    *g.quota.lock().unwrap() =
        r#"{"code":500,"msg":"当前用户不存在coding plan","success":false}"#.into();
    let mut cfg = (*state.config()).clone();
    cfg.providers[0].key = Some("another-fake-glm-key".into());
    state.reload(cfg).unwrap();
    let mut rx = state.bus.subscribe();
    state.refresh_glm_quotas(Duration::from_secs(5)).await;

    assert!(state.quotas().is_empty());
    let windows = loop {
        match tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
            Ok(Ok(tw_api::Event::QuotaSeen {
                provider, windows, ..
            })) if provider == "glm" => break windows,
            Ok(Ok(_)) => continue,
            other => panic!("没等到撤下额度的事件：{other:?}"),
        }
    };
    assert!(windows.is_empty(), "{windows:?}");
    assert_eq!(
        *g.asked.lock().unwrap(),
        ["fake-glm-key", "another-fake-glm-key"]
    );
}

#[tokio::test]
async fn a_used_up_quota_in_a_429_is_reported_and_the_quota_is_asked() {
    let g = Arc::new(Glm::default());
    *g.model.lock().unwrap() = (
        429,
        r#"{"type":"error","error":{"type":"1308","message":"Usage limit reached for 5 hour. Your limit will reset at 2099-10-03 08:23:14"}}"#.into(),
    );
    // 额度接口还没跟上：5 小时窗口只报了 99%，也没说什么时候重置
    *g.quota.lock().unwrap() = r#"{"success":true,"data":{"limits":[{"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":99}]}}"#.into();
    let state = state_for(start_glm(g.clone()).await).await;
    let mut rx = state.bus.subscribe();
    let gw = tw_gateway::serve(state.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"glm-4.6","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 429, "429 要保住 429");

    let (window, resets_at) = loop {
        match tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
            Ok(Ok(tw_api::Event::QuotaExhausted {
                window,
                resets_at_ms,
                ..
            })) => break (window, resets_at_ms),
            Ok(Ok(_)) => continue,
            other => panic!("没等到额度用完的事件：{other:?}"),
        }
    };
    assert_eq!(window, "5h");
    // 额度接口没说，才用消息里的北京时间
    let utc = chrono::DateTime::parse_from_rfc3339("2099-10-03T00:23:14Z").unwrap();
    assert_eq!(resets_at, Some(utc.timestamp_millis() as u64));

    // 有请求去了这一家：顺手问了一次额度
    let asked = async {
        while g.asked.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(3), asked)
        .await
        .expect("有请求时应当去问额度");
}

#[tokio::test]
async fn a_429_that_is_not_the_quota_leaves_the_quota_alone() {
    let g = Arc::new(Glm::default());
    *g.model.lock().unwrap() = (
        429,
        r#"{"type":"error","error":{"type":"1302","message":"High concurrency, slow down"}}"#
            .into(),
    );
    *g.quota.lock().unwrap() = r#"{"code":500,"success":false}"#.into();
    let state = state_for(start_glm(g.clone()).await).await;
    let mut rx = state.bus.subscribe();
    let gw = tw_gateway::serve(state.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"glm-4.6","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 429);
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(e) = rx.try_recv() {
        assert!(
            !matches!(e, tw_api::Event::QuotaExhausted { .. }),
            "临时限流不是额度用完：{e:?}"
        );
    }
    assert!(state.quotas().is_empty());
}
