//! 接管端点的形状（DESIGN.md §7.11）。
//!
//! 这些测试盯的不是「能不能跑通」，而是几条**必须成立的纪律**：
//! plan 不写盘、密钥不回显、还原不拿备份覆盖、以及「已接管」不等于
//! 「已生效」。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

const BASE: &str = "version: 1\nclients:\n  - name: 我\n    key: tw-一把钥匙就够\nproviders:\n  - name: 官方\n    base_url: https://api.anthropic.com\n    key: sk-x\n";

struct Bed {
    _dir: tempfile::TempDir,
    app: axum::Router,
    home: std::path::PathBuf,
}

fn bed() -> Bed {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, BASE).unwrap();
    let cfg: tw_config::Config = serde_yaml_ng::from_str(BASE).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let home = d.path().join("home");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    let state = ControlState {
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        gateway_addr: None,
        store: None,
        started: std::time::Instant::now(),
        // 接管走这个 home。**测试里绝不能碰开发者自己的配置**，而且它
        // 是个字段而不是进程级的 $HOME —— 后者会让并行跑的测试互相踩。
        home: home.clone(),
    };
    Bed {
        app: tw_control::router(state),
        home,
        _dir: d,
    }
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, String) {
    let r = app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let st = r.status();
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (st, String::from_utf8_lossy(&b).to_string())
}

async fn post(app: &axum::Router, path: &str, body: &str) -> (StatusCode, String) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let st = r.status();
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (st, String::from_utf8_lossy(&b).to_string())
}

const CLAUDE: &str = "{\n  \"model\": \"opusplan\",\n  \"env\": { \"MY_OWN\": \"别动我\" }\n}\n";

#[tokio::test]
async fn listing_says_which_clients_are_here_and_which_are_only_advice() {
    let b = bed();
    let (st, body) = get(&b.app, "/clients").await;
    assert_eq!(st, StatusCode::OK);
    let v: tw_api::ClientsResponse = serde_json::from_str(&body).unwrap();
    assert!(v.clients.iter().any(|c| c.id == "claude-code"));
    // **不假装能接管。**Cursor 的 Tab 补全根本不经过我们
    assert!(v.manual.iter().any(|m| m.name == "Cursor"));
    // 客户端要连的是 127.0.0.1，不是监听地址
    assert!(
        v.gateway_base.starts_with("http://127.0.0.1:"),
        "{}",
        v.gateway_base
    );
    assert_eq!(v.keys, vec!["我".to_string()]);
}

#[tokio::test]
async fn a_client_that_needs_a_restart_says_so_and_gets_no_silence_warning() {
    // 用户可能一整天都没重开过终端。那时弹「是不是没生效」是狼来了。
    let b = bed();
    let (_, body) = get(&b.app, "/clients").await;
    let v: tw_api::ClientsResponse = serde_json::from_str(&body).unwrap();
    let codex = v.clients.iter().find(|c| c.id == "codex").unwrap();
    assert_eq!(codex.takes_effect, "on_restart");
    assert!(!codex.warns_when_silent);
    assert!(codex.takes_effect_note.contains("重开"));
}

#[tokio::test]
async fn planning_shows_the_change_without_writing_a_single_byte() {
    let b = bed();
    let p = b.home.join(".claude/settings.json");
    std::fs::write(&p, CLAUDE).unwrap();

    let (st, body) = post(&b.app, "/clients/plan", r#"{"client":"claude-code"}"#).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let v: tw_api::PlanView = serde_json::from_str(&body).unwrap();
    assert!(v.after.contains("ANTHROPIC_BASE_URL"), "{}", v.after);
    assert!(v.after.contains("别动我"), "{}", v.after);
    assert!(!v.noop);
    assert_eq!(
        std::fs::read_to_string(&p).unwrap(),
        CLAUDE,
        "算一下就把文件改了"
    );
}

#[tokio::test]
async fn the_plan_summary_never_echoes_the_key() {
    // 不回显密钥，哪怕是打码的 —— 回显会让「猜密钥」这件事有反馈信号。
    let b = bed();
    std::fs::write(b.home.join(".claude/settings.json"), CLAUDE).unwrap();
    let (_, body) = post(&b.app, "/clients/plan", r#"{"client":"claude-code"}"#).await;
    let v: tw_api::PlanView = serde_json::from_str(&body).unwrap();
    assert!(v.carries_secret);
    for f in &v.fields {
        assert!(!f.contains("tw-一把钥匙就够"), "字段摘要里回显了密钥：{f}");
    }
    assert!(
        v.fields.iter().any(|f| f.contains("网关密钥")),
        "{:?}",
        v.fields
    );
}

#[tokio::test]
async fn adopt_then_restore_puts_the_users_file_back() {
    let b = bed();
    let p = b.home.join(".claude/settings.json");
    std::fs::write(&p, CLAUDE).unwrap();

    let (st, body) = post(&b.app, "/clients/adopt", r#"{"client":"claude-code"}"#).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let a: tw_api::AdoptResponse = serde_json::from_str(&body).unwrap();
    assert!(!a.backup.is_empty(), "没留备份");
    assert!(std::fs::read_to_string(&p).unwrap().contains("127.0.0.1"));

    let (st, body) = post(&b.app, "/clients/claude-code/restore", "").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(std::fs::read_to_string(&p).unwrap(), CLAUDE);
}

#[tokio::test]
async fn adopting_reports_being_adopted_but_never_claims_it_took_effect() {
    // 「已接管」和「已生效」是两回事。**我们改了一个文件，但那个文件
    // 有没有被读到，只有请求能证明**（§7.11）。
    let b = bed();
    std::fs::write(b.home.join(".claude/settings.json"), CLAUDE).unwrap();
    post(&b.app, "/clients/adopt", r#"{"client":"claude-code"}"#).await;

    let (_, body) = get(&b.app, "/clients").await;
    let v: tw_api::ClientsResponse = serde_json::from_str(&body).unwrap();
    let cc = v.clients.iter().find(|c| c.id == "claude-code").unwrap();
    assert!(cc.adopted_at_ms.is_some(), "没记下接管过");
    assert_eq!(cc.last_seen_ms, None, "一个请求都没来过，不该说已生效");
}

#[tokio::test]
async fn restoring_something_we_never_adopted_refuses_instead_of_guessing() {
    let b = bed();
    std::fs::write(b.home.join(".claude/settings.json"), CLAUDE).unwrap();
    let (st, body) = post(&b.app, "/clients/claude-code/restore", "").await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        std::fs::read_to_string(b.home.join(".claude/settings.json")).unwrap(),
        CLAUDE
    );
}

#[tokio::test]
async fn asking_about_a_client_we_do_not_know_is_a_404_not_a_panic() {
    let b = bed();
    let (st, _) = post(&b.app, "/clients/plan", r#"{"client":"没这个"}"#).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = get(&b.app, "/clients/没这个/why").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_diagnosis_hands_over_a_command_rather_than_running_it() {
    let b = bed();
    std::fs::write(
        b.home.join(".zshrc"),
        "export ANTHROPIC_BASE_URL=https://别处\n",
    )
    .unwrap();
    let (st, body) = get(&b.app, "/clients/claude-code/why").await;
    assert_eq!(st, StatusCode::OK);
    let v: Vec<tw_api::FindingView> = serde_json::from_str(&body).unwrap();
    let f = v
        .iter()
        .find(|f| f.title.contains("ANTHROPIC_BASE_URL"))
        .unwrap();
    assert!(f.fix.as_ref().unwrap().contains("sed"), "{:?}", f.fix);
    // 文件还在，我们没动它
    assert!(
        std::fs::read_to_string(b.home.join(".zshrc"))
            .unwrap()
            .contains("export"),
        "我们把用户的 .zshrc 改了"
    );
    // 查干净的项也要说出来（§0.6）
    assert!(v.iter().any(|f| f.level == "clear"), "{v:?}");
}
