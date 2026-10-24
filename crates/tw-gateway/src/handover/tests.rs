use std::time::Duration;

use super::*;
use crate::affinity::{Conversation, Turn};
use crate::key_limits::{Ask, Recorded};

fn parse(yaml: &str) -> Vec<tw_config::KeyLimit> {
    serde_yaml_ng::from_str(yaml).unwrap()
}

fn key_limits() -> Vec<tw_config::KeyLimit> {
    parse("- {per: day, tokens: 100000}")
}

fn upstream_limits() -> Vec<tw_config::KeyLimit> {
    parse("- {per: day, requests: 1000}")
}

/// 一小时之后过期，RFC 3339
fn in_an_hour() -> String {
    (chrono::Utc::now() + chrono::Duration::hours(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn oauth(access: &str) -> tw_config::OAuth {
    tw_config::OAuth {
        access: Some(access.into()),
        expires_at: Some(in_an_hour()),
        refresh: "rt-1".into(),
        endpoint: "https://auth.invalid/token".into(),
        client_id: None,
        client_secret: None,
        refresh_before: None,
    }
}

/// 密钥 `k`（并发上限 `max`、每天 10 万 token），上游 `甲`（OAuth、并发上限 1、每天 1000 个
/// 请求）和 `乙`
fn config(max: usize, access: &str) -> tw_config::Config {
    tw_config::Config {
        clients: vec![tw_config::Client {
            name: "k".into(),
            key: "tw-k".into(),
            max_concurrent: Some(max),
            limits: key_limits(),
            ..Default::default()
        }],
        providers: vec![
            tw_config::Provider {
                name: "甲".into(),
                base_url: "https://a.invalid".into(),
                oauth: Some(oauth(access)),
                max_concurrent: Some(1),
                limits: upstream_limits(),
                ..Default::default()
            },
            tw_config::Provider {
                name: "乙".into(),
                base_url: "https://b.invalid".into(),
                key: Some("sk-b".into()),
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}

/// 旧 core 和新 core，同一份配置
fn pair() -> (AppState, AppState) {
    (
        AppState::new(config(1, "at-1")).unwrap(),
        AppState::new(config(1, "at-1")).unwrap(),
    )
}

/// 过一遍交接连接：写成字符串再读回来
fn over_the_wire(v: serde_json::Value) -> serde_json::Value {
    serde_json::from_str(&v.to_string()).unwrap()
}

/// 让等着的有机会走一步
async fn settle() {
    tokio::time::sleep(Duration::from_millis(30)).await;
}

/// 排空开始：旧 core 报的每一条都交给新 core，像交接连接那样按顺序
fn forward(old: &AppState, new: &AppState) -> tokio::task::JoinHandle<()> {
    let mut rx = old.begin_drain();
    let new = new.clone();
    tokio::spawn(async move {
        while let Some(v) = rx.recv().await {
            new.handover_settled(&over_the_wire(v));
        }
    })
}

fn group() -> tw_engine::Group {
    tw_engine::Group {
        name: "池子".into(),
        kind: tw_engine::GroupType::LoadBalance,
        providers: [("甲", 2), ("乙", 1)]
            .iter()
            .map(|(n, w)| tw_engine::Member {
                name: n.to_string(),
                weight: *w,
            })
            .collect(),
        selected: None,
        balance_by: Default::default(),
    }
}

fn recorded(input: u64, output: u64) -> Recorded {
    Recorded {
        client: "k".into(),
        path: "/v1/messages".into(),
        reached: true,
        requests: 1,
        input,
        output,
        ..Default::default()
    }
}

fn used(s: &AppState) -> u64 {
    s.key_limits.view("k", &key_limits())[0].used
}

fn upstream_used(s: &AppState) -> u64 {
    s.upstream_limits.view("甲", &upstream_limits())[0].used
}

/// 一节，数组按各条的样子排好：两边的 HashMap 顺序不同
fn sorted(snap: &serde_json::Value, name: &str) -> serde_json::Value {
    let mut v = snap[name].clone();
    if let Some(a) = v.as_array_mut() {
        a.sort_by_key(|x| x.to_string());
    }
    v
}

/// **快照往返**：旧 core 导出、新 core 接进来之后，新 core 看到的和旧 core 一样 —— 再从新 core
/// 导出一遍，搬过去的每一节都一字不差；路由、熔断、限流、用量、名额照旧起作用。
#[tokio::test]
async fn a_snapshot_round_trip_leaves_the_new_core_where_the_old_one_was() {
    let old = AppState::new(config(1, "at-1")).unwrap();
    // 新 core 的配置里 access token 是旧的那个（写回还没落地）：接到的缓存让它用旧 core 手上的
    let new = AppState::new(config(1, "at-0")).unwrap();
    let now = crate::server::now_ms();

    let c = Conversation::new(
        "default",
        "对话",
        Turn {
            number: 1,
            within: false,
        },
    );
    old.affinity
        .ticket(&c, Some("池子".into()), "甲")
        .answered(4096);
    assert_eq!(old.affinity.holder(&c, now).as_deref(), Some("甲"));
    old.seals.note("对话", "乙", &[[7u8; 32]], now);
    old.cache_marks.note("甲", "claude-opus", now);
    let session = old.sessions.assign("fp", now);
    for _ in 0..3 {
        old.health.record_failure("乙");
    }
    assert!(!old.health.is_available("乙"));
    old.throttles.limited(
        "甲",
        std::time::Instant::now(),
        Some(Duration::from_secs(120)),
    );
    for ms in [100, 120, 140] {
        old.latency.record("甲", ms);
    }
    old.pace.learn("甲", "m", 100, 150);
    let g = group();
    {
        let members = g.names();
        let t = old.balance.turn(&g);
        let f = tw_engine::Facts {
            current_weight: t.current(),
            ..Default::default()
        };
        t.charge(&members, &f, "甲");
    }
    let (token, _) = old
        .oauth
        .token("甲", &oauth("at-1"), &old.http)
        .await
        .unwrap();
    assert_eq!(token, "at-1");
    // 一个结算过的（150 token），一个在跑的（预留 600）
    let done = old
        .key_limits
        .admit("k", &key_limits(), Ask::default(), Duration::ZERO)
        .await
        .unwrap();
    done.bind(6);
    old.key_limits
        .settle(6, crate::server::now_ms() as i64, &recorded(100, 50));
    let running = old
        .key_limits
        .admit(
            "k",
            &key_limits(),
            Ask {
                tokens: 600,
                cost_micros: 0,
            },
            Duration::ZERO,
        )
        .await
        .unwrap();
    running.bind(7);
    let hop = old
        .upstream_limits
        .reserve("甲", &upstream_limits(), &Ask::default());
    hop.bind(7);
    let _pass = old.gate.acquire("k", Some(1)).await;
    let _slot = old.slots.try_take("甲").unwrap();

    let snap = over_the_wire(old.handover_export());
    new.handover_import(&snap);

    // 搬过去的一字不差
    let again = over_the_wire(new.handover_export());
    for name in [
        wire::AFFINITY,
        wire::SEALS,
        wire::CACHE_MARKS,
        wire::SESSIONS,
        wire::LATENCY,
        wire::PACE,
        wire::BALANCE,
        wire::GATE,
        wire::SLOTS,
    ] {
        assert_eq!(sorted(&again, name), sorted(&snap, name), "{name}");
    }
    assert_eq!(again[wire::HEALTH], snap[wire::HEALTH]);
    let books = |s: &serde_json::Value, n: &str| {
        let mut b = s[n]["books"].clone();
        b.as_array_mut().unwrap().sort_by_key(|x| x.to_string());
        b
    };
    assert_eq!(
        books(&again, wire::KEY_LIMITS),
        books(&snap, wire::KEY_LIMITS)
    );
    assert_eq!(
        books(&again, wire::UPSTREAM_LIMITS),
        books(&snap, wire::UPSTREAM_LIMITS)
    );
    assert_eq!(again[wire::KEY_LIMITS]["held"].as_array().unwrap().len(), 1);
    assert_eq!(again[wire::OAUTH][0]["access"], "at-1");
    let left = |s: &serde_json::Value| s[wire::THROTTLES][0]["pause_left_ms"].as_u64().unwrap();
    assert!(left(&again) <= left(&snap) && left(&snap) - left(&again) < 2_000);

    // 照旧起作用
    let mut candidates = vec!["乙".to_string(), "甲".to_string()];
    let stay = new
        .affinity
        .stay(&c, Some("池子"), &mut candidates, |_| true, now);
    assert_eq!(stay, Some(tw_api::Stay::Cache));
    assert_eq!(candidates[0], "甲");
    assert_eq!(new.affinity.holder(&c, now).as_deref(), Some("甲"));
    assert!(new.seals.of("对话", "乙").unwrap().contains(&[7u8; 32]));
    assert!(new.cache_marks.refused("甲", "claude-opus"));
    assert_eq!(new.sessions.assign("fp", now + 1), session);
    assert!(!new.health.is_available("乙"), "熔断中的上游又被放行了");
    let paused = new.throttles.view("甲").expect("限流暂停丢了");
    assert_eq!(paused.kind, tw_api::ThrottleKind::RateLimited);
    assert!(paused.until_ms > now + 100_000);
    assert_eq!(new.latency.typical("甲"), Some(120));
    assert_eq!(new.pace.ratio("甲", "m"), 1.5);
    assert_eq!(new.balance.peek(&g), old.balance.peek(&g));
    let (token, renewed) = new
        .oauth
        .token("甲", &oauth("at-0"), &new.http)
        .await
        .unwrap();
    assert_eq!(token, "at-1", "新 core 没用上旧 core 手上的 access token");
    assert!(renewed.is_none());
    assert_eq!(used(&new), 750, "结算过的 150 加在跑的预留 600");
    assert_eq!(upstream_used(&new), 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), new.gate.acquire("k", Some(1)))
            .await
            .is_err(),
        "旧 core 占着的密钥名额没算上"
    );
    assert!(
        new.slots.try_take("甲").is_none(),
        "旧 core 占着的上游位置没算上"
    );
}

/// **V5：密钥并发上限 1 时，排空期两个进程合计不超过 1。**旧 core 一个在跑、一个排着队；新 core
/// 新来的请求等到旧 core 的两个都结束 —— 旧 core 那个排着队的先进去，新的不能趁前一个结束时
/// 一起进去。
#[tokio::test]
async fn v5_with_a_limit_of_one_the_two_cores_together_never_run_two() {
    let (old, new) = pair();
    let running = old.gate.acquire("k", Some(1)).await;
    let o = old.clone();
    let queued = tokio::spawn(async move { o.gate.acquire("k", Some(1)).await });
    settle().await;

    new.handover_import(&over_the_wire(old.handover_export()));
    let _fwd = forward(&old, &new);
    let n = new.clone();
    let fresh = tokio::spawn(async move { n.gate.acquire("k", Some(1)).await });
    settle().await;
    assert!(
        !fresh.is_finished(),
        "旧 core 有一个在跑，新 core 又放进去一个"
    );

    drop(running);
    settle().await;
    assert!(queued.is_finished(), "旧 core 排着的那一个该进去了");
    assert!(
        !fresh.is_finished(),
        "旧 core 排着的那一个刚进去，新 core 的也进去了：合计 2"
    );

    drop(queued.await.unwrap());
    settle().await;
    assert!(fresh.is_finished(), "旧 core 都结束了，新 core 的还在等");
    let _p = fresh.await.unwrap();
    // 名额没有因为交接多出来或者少掉
    assert!(
        tokio::time::timeout(Duration::from_millis(50), new.gate.acquire("k", Some(1)))
            .await
            .is_err()
    );
}

/// 旧 core 排着队、客户端走了：那个名额跟着放掉，新 core 不用等一个永远不会结束的请求
#[tokio::test]
async fn a_request_that_gave_up_while_queued_in_the_old_core_frees_its_place() {
    let (old, new) = pair();
    let running = old.gate.acquire("k", Some(1)).await;
    let o = old.clone();
    let queued = tokio::spawn(async move { o.gate.acquire("k", Some(1)).await });
    settle().await;
    new.handover_import(&over_the_wire(old.handover_export()));
    let _fwd = forward(&old, &new);

    queued.abort();
    settle().await;
    let n = new.clone();
    let fresh = tokio::spawn(async move { n.gate.acquire("k", Some(1)).await });
    settle().await;
    assert!(!fresh.is_finished(), "旧 core 还有一个在跑");
    drop(running);
    settle().await;
    assert!(fresh.is_finished());
}

/// 交接连接断了：替旧 core 占着的名额和预留一并放掉
#[tokio::test]
async fn release_all_gives_back_everything_held_for_the_old_core() {
    let (old, new) = pair();
    let _pass = old.gate.acquire("k", Some(1)).await;
    let _slot = old.slots.try_take("甲").unwrap();
    let hold = old
        .key_limits
        .admit(
            "k",
            &key_limits(),
            Ask {
                tokens: 600,
                cost_micros: 0,
            },
            Duration::ZERO,
        )
        .await
        .unwrap();
    hold.bind(7);
    new.handover_import(&over_the_wire(old.handover_export()));
    assert_eq!(used(&new), 600);

    new.handover_release_all();
    tokio::time::timeout(Duration::from_millis(200), new.gate.acquire("k", Some(1)))
        .await
        .expect("放掉之后还在等");
    assert!(new.slots.try_take("甲").is_some());
    assert_eq!(used(&new), 0);
}

/// 旧 core 占着的比新 core 的上限还多（旧 core 那边上限是 2）：多出来的记成欠着，新 core 等它们
/// 全部结束；结束之后新 core 的上限照旧是 1，不多不少
#[tokio::test]
async fn more_held_than_the_new_limit_is_carried_as_debt() {
    let old = AppState::new(config(2, "at-1")).unwrap();
    let new = AppState::new(config(1, "at-1")).unwrap();
    let a = old.gate.acquire("k", Some(2)).await;
    let b = old.gate.acquire("k", Some(2)).await;
    new.handover_import(&over_the_wire(old.handover_export()));
    let _fwd = forward(&old, &new);
    let n = new.clone();
    let fresh = tokio::spawn(async move { n.gate.acquire("k", Some(1)).await });
    settle().await;
    drop(a);
    settle().await;
    assert!(!fresh.is_finished(), "旧 core 还有一个在跑");
    drop(b);
    settle().await;
    assert!(fresh.is_finished());
    let _held = fresh.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), new.gate.acquire("k", Some(1)))
            .await
            .is_err(),
        "欠账还完之后上限变大了"
    );
}

/// 上游的位置：旧 core 那一跳结束才空出来
#[tokio::test]
async fn an_upstream_place_held_by_the_old_core_frees_when_its_hop_ends() {
    let (old, new) = pair();
    let slot = old.slots.try_take("甲").unwrap();
    new.handover_import(&over_the_wire(old.handover_export()));
    assert!(new.slots.try_take("甲").is_none());
    let _fwd = forward(&old, &new);
    settle().await;
    assert!(new.slots.try_take("甲").is_none());
    drop(slot);
    settle().await;
    assert!(new.slots.try_take("甲").is_some());
}

/// 用量：预留在旧 core 结算时换成实数；快照之后才占上的预留、之后退回的，新 core 都跟着变
#[tokio::test]
async fn reservations_turn_into_real_usage_when_the_old_core_settles() {
    let (old, new) = pair();
    let ask = |tokens| Ask {
        tokens,
        cost_micros: 0,
    };
    let running = old
        .key_limits
        .admit("k", &key_limits(), ask(600), Duration::ZERO)
        .await
        .unwrap();
    running.bind(7);
    let hop = old
        .upstream_limits
        .reserve("甲", &upstream_limits(), &Ask::default());
    hop.bind(7);
    new.handover_import(&over_the_wire(old.handover_export()));
    assert_eq!(used(&new), 600);
    assert_eq!(upstream_used(&new), 1);

    // 快照之后、排空之前：又一个准入了（还没开始），之后被丢掉
    let late = old
        .key_limits
        .admit("k", &key_limits(), ask(300), Duration::ZERO)
        .await
        .unwrap();
    let fwd = forward(&old, &new);
    settle().await;
    assert_eq!(used(&new), 900, "快照之后占上的预留没跟过来");
    drop(late);
    settle().await;
    assert_eq!(used(&new), 600, "退回的预留还算着");

    let at = crate::server::now_ms() as i64;
    old.key_limits.settle(7, at, &recorded(100, 50));
    old.upstream_limits
        .settle(7, at, &["甲".to_string()], Some("甲"), &recorded(100, 50));
    settle().await;
    assert_eq!(used(&new), 150, "预留没换成实数");
    assert_eq!(upstream_used(&new), 1);
    assert_eq!(used(&old), 150);
    fwd.abort();
}

/// 一段接一段地交（H8）：替更早的 core 占着的名额，交给下一个时照样算上
#[tokio::test]
async fn names_held_for_an_earlier_core_are_handed_on() {
    let (a, b) = pair();
    let _pass = a.gate.acquire("k", Some(1)).await;
    b.handover_import(&over_the_wire(a.handover_export()));
    let snap = b.handover_export();
    assert_eq!(snap[wire::GATE]["k"], 1);
}

/// 认不出的部分丢掉，不 panic：读不懂的一节、一节里读不懂的一条、不认识的一节
#[tokio::test]
async fn unreadable_parts_of_a_snapshot_are_left_out() {
    let (_, new) = pair();
    let snap = serde_json::json!({
        "at_ms": "昨天",
        "health": "坏了",
        "pace": [{"upstream": "甲", "model": "m", "ratio": 1.25}, {"什么": 1}],
        "gate": {"k": "一"},
        "以后才有的": [1, 2, 3],
    });
    new.handover_import(&snap);
    new.handover_import(&serde_json::json!("not even an object"));
    assert_eq!(new.pace.ratio("甲", "m"), 1.25);
    new.handover_settled(&serde_json::json!({"k": "以后才有的"}));
    new.handover_settled(&serde_json::json!(42));
    new.handover_settled(&serde_json::json!({"k": "key_settle", "seq": 99, "at_ms": 0, "rec": {}}));
}

/// 快照之后迟迟没有排空、攒的变化太多：不再攒，排空开始时照样报一份此刻占着的全部
#[tokio::test]
async fn a_feed_that_overflowed_is_replaced_by_a_full_report_at_drain() {
    let (feed, _rx) = feed::Feed::new();
    for _ in 0..=20_000 {
        feed.send(&wire::Event::KeyDrop { seq: 0 });
    }
    assert!(feed.lost());

    let (old, new) = pair();
    let hold = old
        .key_limits
        .admit(
            "k",
            &key_limits(),
            Ask {
                tokens: 600,
                cost_micros: 0,
            },
            Duration::ZERO,
        )
        .await
        .unwrap();
    hold.bind(7);
    let _ = old.handover_export();
    // 当成攒满了
    if let Some((f, _)) = old.handover.pending.lock().unwrap().as_ref() {
        f.mark_lost();
    }
    let _pass = old.gate.acquire("k", Some(1)).await;
    let _fwd = forward(&old, &new);
    settle().await;
    assert_eq!(used(&new), 600, "排空开始时没报在跑的预留");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), new.gate.acquire("k", Some(1)))
            .await
            .is_err(),
        "排空开始时没报占着的名额"
    );
}

/// 快照来两份（旧 core 交出监听之前再导出一次，那时它已暂停续期）：**后一份的 OAuth 盖掉
/// 前一份的**，新 core 用的是旧 core 最后手上的那个 access token
#[tokio::test]
async fn a_later_snapshot_replaces_the_oauth_tokens_of_an_earlier_one() {
    let new = AppState::new(config(1, "at-1")).unwrap();
    // 第一份：旧 core 手上是 at-1
    let first = AppState::new(config(1, "at-1")).unwrap();
    first
        .oauth
        .token("甲", &oauth("at-1"), &first.http)
        .await
        .unwrap();
    new.handover_import(&over_the_wire(first.handover_export()));
    // 第二份：之间旧 core 换到了 at-2（同一份配置、同一个 refresh token）
    let second = AppState::new(config(1, "at-2")).unwrap();
    second
        .oauth
        .token("甲", &oauth("at-2"), &second.http)
        .await
        .unwrap();
    new.handover_import(&over_the_wire(second.handover_export()));
    new.oauth.hold_renewals(true);
    let (token, _) = new
        .oauth
        .token("甲", &oauth("at-1"), &new.http)
        .await
        .unwrap();
    assert_eq!(token, "at-2", "后一份快照没盖掉前一份");
}
