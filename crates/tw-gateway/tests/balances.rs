//! 上游的余额：认出是哪一种中转站、读、读不成时留着上一次的、请求结束时记一笔。
//!
//! **全程只对本机的假服务器**：每一种余额接口的回答照它们的字段写，密钥是编的。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::IntoResponse;
use axum::routing::any;
use tw_api::{BalanceScope, BalanceSource, Event, Money, Spent, SpentPeriod};
use tw_config::{BalanceSetting, Client, Config, Provider};

/// 收到的一个余额请求
#[derive(Debug, Clone)]
struct Seen {
    path: String,
    query: Option<String>,
    auth: String,
    user_agent: String,
    accept: String,
}

/// 一台假服务器：每个路径回什么（没写的是 404），收到过什么
#[derive(Default)]
struct Fake {
    answers: Mutex<HashMap<String, (u16, String)>>,
    seen: Mutex<Vec<Seen>>,
}

impl Fake {
    fn answer(&self, path: &str, status: u16, body: &str) {
        self.answers
            .lock()
            .unwrap()
            .insert(path.to_string(), (status, body.to_string()));
    }
    fn paths(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.path.clone())
            .collect()
    }
}

async fn handle(State(f): State<Arc<Fake>>, uri: Uri, h: HeaderMap) -> axum::response::Response {
    let header = |name: &str| {
        h.get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    // 模型请求不算余额请求
    if uri.path().ends_with("/v1/messages") {
        return (
            [("content-type", "application/json")],
            r#"{"type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":3,"output_tokens":1}}"#,
        )
            .into_response();
    }
    f.seen.lock().unwrap().push(Seen {
        path: uri.path().to_string(),
        query: uri.query().map(str::to_string),
        auth: header("authorization"),
        user_agent: header("user-agent"),
        accept: header("accept"),
    });
    let (status, body) = f
        .answers
        .lock()
        .unwrap()
        .get(uri.path())
        .cloned()
        .unwrap_or((404, r#"{"error":"not found"}"#.to_string()));
    (
        axum::http::StatusCode::from_u16(status).unwrap(),
        [("content-type", "application/json")],
        body,
    )
        .into_response()
}

async fn start(f: Arc<Fake>) -> SocketAddr {
    let app = Router::new().fallback(any(handle)).with_state(f);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

/// 一家指向假服务器的上游。地址带着 `/anthropic`：余额接口接在源后面，不接在它后面
fn state_for(addr: SocketAddr, balance: BalanceSetting) -> tw_gateway::AppState {
    tw_gateway::AppState::new(Config {
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "relay".into(),
            base_url: format!("http://{addr}/anthropic"),
            key: Some("sk-relay-fake".into()),
            protocol: Some(tw_config::Protocol::Anthropic),
            balance,
            ..Default::default()
        }],
        ..Default::default()
    })
    .unwrap()
}

/// 到了时候的都读完
async fn read_due(state: &tw_gateway::AppState) {
    for h in state.start_due_balances() {
        tokio::time::timeout(Duration::from_secs(15), h)
            .await
            .expect("a read took too long")
            .unwrap();
    }
}

fn balance(state: &tw_gateway::AppState) -> Option<tw_api::Balance> {
    state.balance_of(&state.config().providers[0])
}

/// 总线上已经有的 `balance_updated`
fn updates(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> usize {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter(|e| matches!(e, Event::BalanceUpdated { provider, .. } if provider == "relay"))
        .count()
}

const SUB2API_WALLET: &str = r#"{"mode":"unrestricted","isValid":true,"planName":"钱包余额","unit":"USD","balance":12.3456,"remaining":12.3456,
    "usage":{"today":{"requests":3,"input_tokens":1200,"output_tokens":300,"cost":0.12},"total":{"requests":90,"cost":7.65}},
    "daily_usage":[{"date":"2026-10-10","requests":3,"cost":0.12}],
    "model_stats":[{"model":"claude-sonnet-4-5","requests":3,"cost":0.12}]}"#;

#[tokio::test]
async fn auto_finds_a_sub2api_relay_and_reads_its_wallet_from_the_same_answer() {
    let f = Arc::new(Fake::default());
    f.answer("/v1/usage", 200, SUB2API_WALLET);
    let state = state_for(start(f.clone()).await, BalanceSetting::Auto);
    let mut rx = state.bus.subscribe();

    read_due(&state).await;
    let b = balance(&state).expect("a balance");
    assert_eq!(b.source, BalanceSource::Sub2api);
    assert_eq!(
        b.wallet,
        Some(Money {
            amount: 12.3456,
            currency: "USD".into()
        })
    );
    assert_eq!(
        b.spent,
        Some(Spent {
            amount: 0.12,
            currency: "USD".into(),
            period: SpentPeriod::Today,
            scope: None,
        })
    );
    assert_eq!(b.error, None);
    assert_eq!(updates(&mut rx), 1);

    // **认出来的那一次回答就是余额**：只问了一次。凭据是这家的密钥，按 Bearer 发；
    // User-Agent 是 ThinkWatch 的
    let seen = f.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].path, "/v1/usage");
    assert_eq!(seen[0].auth, "Bearer sk-relay-fake");
    assert!(seen[0].user_agent.starts_with("thinkwatch/"), "{seen:?}");
    assert_eq!(seen[0].accept, "application/json");

    // 刚读过：不再读
    read_due(&state).await;
    assert_eq!(f.paths().len(), 1);
}

#[tokio::test]
async fn auto_finds_a_new_api_relay_and_reads_its_quota() {
    let f = Arc::new(Fake::default());
    f.answer(
        "/v1/dashboard/billing/subscription",
        200,
        r#"{"object":"billing_subscription","has_payment_method":true,"soft_limit_usd":25,"hard_limit_usd":25,"system_hard_limit_usd":25,"access_until":0}"#,
    );
    f.answer(
        "/v1/dashboard/billing/usage",
        200,
        r#"{"object":"list","total_usage":1234.5}"#,
    );
    let state = state_for(start(f.clone()).await, BalanceSetting::Auto);

    read_due(&state).await;
    let b = balance(&state).expect("a balance");
    assert_eq!(b.source, BalanceSource::Newapi);
    let q = b.quota.unwrap();
    assert_eq!((q.limit, q.used, q.unit.as_str()), (25.0, 12.345, "USD"));
    assert_eq!(b.wallet, None);
    assert_eq!(
        f.paths(),
        [
            "/v1/usage",
            "/v1/dashboard/billing/subscription",
            "/v1/dashboard/billing/usage"
        ]
    );
    let usage = f.seen.lock().unwrap()[2].query.clone().unwrap_or_default();
    assert!(
        usage.contains("start_date=") && usage.contains("end_date="),
        "{usage}"
    );
}

/// 两种都不是：没有余额，**不再问**，请求结束了也不问。只有界面要的时候再问一次
#[tokio::test]
async fn a_host_that_is_neither_has_no_balance_and_is_not_asked_again() {
    let f = Arc::new(Fake::default());
    let state = state_for(start(f.clone()).await, BalanceSetting::Auto);
    let mut rx = state.bus.subscribe();

    read_due(&state).await;
    assert_eq!(balance(&state), None);
    assert_eq!(f.paths().len(), 2, "{:?}", f.paths());
    for _ in 0..5 {
        state.balances.note_request("relay");
        read_due(&state).await;
    }
    assert_eq!(f.paths().len(), 2, "又去问了：{:?}", f.paths());
    assert_eq!(updates(&mut rx), 0, "没有余额，不用报");

    assert_eq!(state.refresh_balance("relay").await, None);
    assert_eq!(f.paths().len(), 4, "界面要的时候再问一次");
}

#[tokio::test]
async fn an_explicit_source_reads_on_the_origin_not_on_the_base_path() {
    let f = Arc::new(Fake::default());
    f.answer(
        "/user/balance",
        200,
        r#"{"is_available":true,"balance_infos":[{"currency":"USD","total_balance":"0.00"},{"currency":"CNY","total_balance":"110.00"}]}"#,
    );
    let state = state_for(start(f.clone()).await, BalanceSetting::Deepseek);

    read_due(&state).await;
    assert_eq!(f.paths(), ["/user/balance"], "不是 /anthropic/user/balance");
    let b = balance(&state).unwrap();
    assert_eq!(b.source, BalanceSource::Deepseek);
    assert_eq!(
        b.wallet,
        Some(Money {
            amount: 110.0,
            currency: "CNY".into()
        })
    );
}

#[tokio::test]
async fn openrouter_without_a_key_limit_reads_the_account_credits() {
    let f = Arc::new(Fake::default());
    f.answer(
        "/api/v1/key",
        200,
        r#"{"data":{"label":"sk-or-v1-fak...e","limit":null,"limit_remaining":null,"usage":3.1}}"#,
    );
    f.answer(
        "/api/v1/credits",
        200,
        r#"{"data":{"total_credits":20,"total_usage":12.5}}"#,
    );
    let state = state_for(start(f.clone()).await, BalanceSetting::Openrouter);

    read_due(&state).await;
    assert_eq!(f.paths(), ["/api/v1/key", "/api/v1/credits"]);
    assert_eq!(balance(&state).unwrap().wallet.unwrap().amount, 7.5);
}

const ENTERPRISE: &str = r#"{"usage":{"requests_today":3,"tokens_today":9000,"requests_month":80,"tokens_month":400000,"cost_usd_month":12.5},
    "limits":[{"scope":"key","kind":"requests","window":"1h","window_secs":3600,"limit":100,"used":7,"resets_at":null},
              {"scope":"user","kind":"tokens","window":"daily","window_secs":null,"limit":1000000,"used":250000,"resets_at":null}],
    "expires_at":null}"#;

/// 企业网关回了 503：**留着上一次读到的**，记下原因；之后读成了，原因清掉
#[tokio::test]
async fn a_failing_read_keeps_the_last_good_reading_and_says_why() {
    let f = Arc::new(Fake::default());
    f.answer("/v1/usage", 200, ENTERPRISE);
    let state = state_for(start(f.clone()).await, BalanceSetting::Thinkwatch);
    let mut rx = state.bus.subscribe();

    let first = state.refresh_balance("relay").await.unwrap();
    assert_eq!(first.source, BalanceSource::Thinkwatch);
    assert_eq!(first.windows.len(), 2);
    assert_eq!(first.windows[1].scope, Some(BalanceScope::User));
    assert_eq!(first.spent.as_ref().unwrap().period, SpentPeriod::Month);

    f.answer("/v1/usage", 503, r#"{"error":"busy"}"#);
    let failed = state.refresh_balance("relay").await.unwrap();
    let why = failed.error.clone().expect("the reason");
    assert_eq!(why.code, "gw.balance.status");
    assert_eq!(why.arg("status"), "503");
    assert_eq!(failed.windows, first.windows, "上一次读到的留着");
    assert_eq!(failed.spent, first.spent);
    assert_eq!(failed.read_at_ms, first.read_at_ms, "还是那一次读到的时刻");
    assert_eq!(balance(&state), Some(failed));

    f.answer("/v1/usage", 200, ENTERPRISE);
    let again = state.refresh_balance("relay").await.unwrap();
    assert_eq!(again.error, None);
    assert_eq!(updates(&mut rx), 3, "每读一次报一次");
}

/// 读不懂的回答、拒绝密钥：原因各说各的
#[tokio::test]
async fn the_reason_says_what_went_wrong() {
    let f = Arc::new(Fake::default());
    f.answer("/user/balance", 200, r#"<html>maintenance</html>"#);
    let state = state_for(start(f.clone()).await, BalanceSetting::Deepseek);
    let b = state.refresh_balance("relay").await.unwrap();
    assert_eq!(b.error.unwrap().code, "gw.balance.unrecognized");
    assert_eq!(b.wallet, None);

    f.answer("/user/balance", 401, r#"{"error":"bad key"}"#);
    let b = state.refresh_balance("relay").await.unwrap();
    let why = b.error.unwrap();
    assert_eq!(why.code, "gw.balance.rejected");
    assert!(!why.text.contains("sk-relay-fake"));
}

/// 换了 `balance:`：之前读到的不再作数，从头来
#[tokio::test]
async fn changing_the_setting_starts_over() {
    let f = Arc::new(Fake::default());
    f.answer("/v1/usage", 200, SUB2API_WALLET);
    let state = state_for(start(f.clone()).await, BalanceSetting::Sub2api);
    read_due(&state).await;
    assert!(balance(&state).is_some());

    let mut cfg = (*state.config()).clone();
    cfg.providers[0].balance = BalanceSetting::Off;
    state.reload(cfg).unwrap();
    assert_eq!(balance(&state), None);
    read_due(&state).await;
    assert_eq!(f.paths().len(), 1, "关掉了就不读");
}

/// 一个请求经过这一家、结束了：叫醒后台（到了时候就读，见 tracker 的测试）
#[tokio::test]
async fn a_request_through_the_upstream_wakes_the_balance_reader() {
    let f = Arc::new(Fake::default());
    f.answer("/v1/usage", 200, SUB2API_WALLET);
    let state = state_for(start(f.clone()).await, BalanceSetting::Sub2api);
    read_due(&state).await;
    let gw = tw_gateway::serve(state.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();

    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-sonnet-4-5","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    let _ = r.bytes().await.unwrap();

    tokio::time::timeout(Duration::from_secs(5), state.balances.wake.notified())
        .await
        .expect("the end of the request did not reach the balance reader");
    // 刚读过：一分钟之内不因为这个请求再读
    read_due(&state).await;
    assert_eq!(f.paths(), ["/v1/usage"]);
}
