//! `POST /handover/arm` 与 `Status.handover` / `draining`（升级不断线）。
//!
//! 交接本身在 `twcore` 里；这里盯控制面这一边说的话：哪些情形拒绝、拒绝时的码（桌面端
//! 据此退回现行流程），答应时交给 `twcore` 的参数，以及 `/status` 报的两个数。

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

const BASE: &str = "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: 我\n    key: tw-一把钥匙就够\nproviders: []\n";

fn state() -> (tempfile::TempDir, ControlState) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, BASE).unwrap();
    let cfg: tw_config::Config = serde_yaml_ng::from_str(BASE).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let state = ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        handover: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        store: None,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    };
    (d, state)
}

async fn arm(s: &ControlState, body: &str) -> (StatusCode, serde_json::Value) {
    let resp = tw_control::router(s.clone())
        .oneshot(
            Request::post("/handover/arm")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn status(s: &ControlState) -> tw_api::Status {
    let resp = tw_control::router(s.clone())
        .oneshot(Request::get("/status").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn arming_hands_the_request_to_the_core_and_answers_with_the_handover_version() {
    let (_d, s) = state();
    let mut arms = s.handover.arm_requests();
    let core = tokio::spawn(async move {
        let a = arms.recv().await.unwrap();
        let got = (a.wait, a.relaunch.clone());
        a.reply.send(Ok(())).unwrap();
        got
    });
    let (code, body) = arm(
        &s,
        r#"{"wait_secs":null,"relaunch":{"kind":"bundle_id","id":"app.thinkwatch.lite"}}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let armed: tw_api::HandoverArmed = serde_json::from_value(body).unwrap();
    assert_eq!(armed.version, tw_handover::VERSION);
    let (wait, relaunch) = core.await.unwrap();
    assert_eq!(wait, Duration::from_secs(120), "缺省等 120 秒");
    assert_eq!(
        relaunch,
        Some(tw_api::Relaunch::BundleId {
            id: "app.thinkwatch.lite".into()
        })
    );
}

#[tokio::test]
async fn a_core_without_a_data_plane_refuses_with_safe_mode() {
    let (_d, s) = state();
    let _arms = s.handover.arm_requests();
    s.handover.set_no_data_plane(true);
    let (code, body) = arm(&s, r#"{"wait_secs":5,"relaunch":null}"#).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "control.handover.safe_mode", "{body}");
}

#[tokio::test]
async fn a_core_that_does_not_run_handovers_refuses_rather_than_pretending() {
    // 没有人接 arm（比如测试里、或者这个进程不跑交接）：说清楚，不假装答应了
    let (_d, s) = state();
    let (code, body) = arm(&s, r#"{"wait_secs":5,"relaunch":null}"#).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "control.handover.safe_mode", "{body}");
}

#[tokio::test]
async fn a_second_handover_while_one_is_taking_over_is_busy() {
    let (_d, s) = state();
    let _arms = s.handover.arm_requests();
    s.handover.set_phase(tw_api::HandoverState::TakingOver);
    let (code, body) = arm(&s, r#"{"wait_secs":5,"relaunch":null}"#).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "control.handover.busy", "{body}");
}

#[tokio::test]
async fn an_endpoint_that_cannot_open_is_reported_with_its_reason() {
    let (_d, s) = state();
    let mut arms = s.handover.arm_requests();
    tokio::spawn(async move {
        let a = arms.recv().await.unwrap();
        let _ = a
            .reply
            .send(Err(tw_control::handover::endpoint_failed("disk full")));
    });
    let (code, body) = arm(&s, r#"{"wait_secs":5,"relaunch":null}"#).await;
    assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["code"], "control.handover.endpoint_failed", "{body}");
}

#[tokio::test]
async fn status_reports_the_handover_phase_and_the_draining_count() {
    let (_d, s) = state();
    let st = status(&s).await;
    assert_eq!(st.handover, tw_api::HandoverState::Idle);
    assert_eq!(st.draining, 0);
    s.handover.set_phase(tw_api::HandoverState::Draining);
    s.handover.set_draining(3);
    let st = status(&s).await;
    assert_eq!(st.handover, tw_api::HandoverState::Draining);
    assert_eq!(st.draining, 3);
    assert_eq!(st.api_version, 54);
}

/// arm 之后（以及接替者正在接管时）不开始新的账号登录：回调端口不交接。（平常、排空中的
/// 新 core 照常能登：见 `handover::tests` 里那条，这里不真的去登）
#[tokio::test]
async fn a_sign_in_that_starts_after_arming_is_refused() {
    let (_d, s) = state();
    for phase in [
        tw_api::HandoverState::Armed,
        tw_api::HandoverState::TakingOver,
    ] {
        s.handover.set_phase(phase);
        for path in ["/chatgpt/login", "/zai/login"] {
            let resp = tw_control::router(s.clone())
                .oneshot(
                    Request::post(path)
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = resp.status();
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            assert_eq!(status, StatusCode::CONFLICT, "{path} {phase:?}: {body}");
            assert_eq!(body["code"], "control.handover.login_refused", "{body}");
        }
    }
}
