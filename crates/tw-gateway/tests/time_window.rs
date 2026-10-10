//! 按时间分流：规则的 `time` 条件按路由那一刻 core 的本地时间求值。
//!
//! 时钟是定死的（[`tw_gateway::AppState::set_local_time`]）：同一份配置，周三上午和
//! 周六上午各起一个网关，同一个请求走到不同的上游。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::any;
use tw_config::{Client, Config, Provider};
use tw_engine::{LocalTime, RouteSet, Rule};

/// 一家上游，回答里带着自己的名字
async fn upstream(name: &'static str) -> SocketAddr {
    let app = Router::new().fallback(any(move || async move {
        axum::response::Response::builder()
            .header("content-type", "application/json")
            .body(axum::body::Body::from(format!(
                r#"{{"type":"message","role":"assistant","model":"m","content":[{{"type":"text","text":"{name}"}}],"usage":{{"input_tokens":3,"output_tokens":1}}}}"#
            )))
            .unwrap()
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

fn provider(name: &str, up: SocketAddr) -> Provider {
    Provider {
        name: name.into(),
        base_url: format!("http://{up}"),
        key: Some("sk-x".into()),
        protocol: Some(tw_config::Protocol::Anthropic),
        ..Default::default()
    }
}

fn rule(name: &str, when: &str, to: &str) -> Rule {
    Rule {
        name: name.into(),
        when: serde_yaml_ng::from_str(when).unwrap(),
        to: Some(to.into()),
        set: None,
        deny: None,
    }
}

fn cfg(day: SocketAddr, night: SocketAddr) -> Config {
    Config {
        clients: vec![Client {
            name: "我".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![provider("day", day), provider("night", night)],
        routes: vec![RouteSet::default_with(vec![
            rule("工作时间", "{ time: 'mon-fri 09:00-18:00' }", "day"),
            rule("其余", "{}", "night"),
        ])],
        ..Default::default()
    }
}

/// 起一个网关，路由看到的本地时间定在 `at`
async fn serve(cfg: Config, at: LocalTime) -> SocketAddr {
    let mut state = tw_gateway::AppState::new(cfg).unwrap();
    state.set_local_time(Arc::new(move || at));
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

/// 发一个请求，返回回答的正文
async fn ask(gw: SocketAddr) -> String {
    let r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"m","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    r.text().await.unwrap()
}

#[tokio::test]
async fn the_same_request_goes_elsewhere_outside_office_hours() {
    let (day, night) = (upstream("day").await, upstream("night").await);
    // 周三 10:00：工作时间，走 day
    let gw = serve(cfg(day, night), LocalTime::at(2, 10, 0)).await;
    let body = ask(gw).await;
    assert!(body.contains(r#""text":"day""#), "{body}");
    // 周六 10:00：不在工作时间，走 night
    let gw = serve(cfg(day, night), LocalTime::at(5, 10, 0)).await;
    let body = ask(gw).await;
    assert!(body.contains(r#""text":"night""#), "{body}");
    // 周三 18:00：终点不含，走 night
    let gw = serve(cfg(day, night), LocalTime::at(2, 18, 0)).await;
    let body = ask(gw).await;
    assert!(body.contains(r#""text":"night""#), "{body}");
}
