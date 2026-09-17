//! M3 的验收标准。
//!
//! 原文四条：**能回答「昨天花了多少钱」「哪家 TTFT 最差」「缓存省了
//! 多少」；混入订阅型上游后金额不失真；菜单栏花费从两位数跳三位数时
//! 右边图标不移动；实测 UI 关窗后的常驻内存。**
//!
//! 后两条不在这一层：菜单栏那条在桌面版（它渲染的是像素），内存那条是
//! 一次性实测。这个文件盯住前四条里属于数据层的。
//!
//! **为什么要有这个文件**：验收标准值得写成测试 —— 手动清单只会在里程碑
//! 那天跑一次，之后每一次改动都可能悄悄破坏它们。M1、M2、M5 都有，M3
//! 一直没有。

use tw_store::db::{Db, RequestRow};

fn open() -> (tempfile::TempDir, Db) {
    let d = tempfile::tempdir().unwrap();
    let db = Db::open(&d.path().join("t.db")).unwrap();
    (d, db)
}

const DAY: i64 = 24 * 3600 * 1000;
const NOW: i64 = 1_800_000_000_000;

fn req(id: i64, at_ms: i64) -> RequestRow {
    RequestRow {
        id,
        at_ms,
        client: "claude-code".into(),
        client_hint: None,
        session: None,
        tool_calls: None,
        flagged: None,
        redacted: None,
        provider: "官方".into(),
        model: "claude-sonnet-4-5".into(),
        path: "/v1/messages".into(),
        status: Some(200),
        ttfb_ms: Some(800),
        duration_ms: Some(4000),
        bytes: Some(1000),
        input_tokens: Some(1000),
        output_tokens: Some(500),
        cache_read_tokens: Some(200),
        cache_write_tokens: None,
        cost_micros: Some(3_000),
        cost_estimated: false,
        error: None,
        local: false,
        cancelled: false,
        routing: None,
        billing: "per-token".into(),
        cache_saved_micros: Some(1_500),
        price_source: None,
        translated: None,
    }
}

/// 验收一：**昨天花了多少钱。**
///
/// 「昨天」不是「最近 24 小时」—— 按天记账的人问的是那一天，而把今天
/// 的钱算进去，得到的是一个他对不上的数字。
#[test]
fn one_you_can_ask_what_yesterday_cost() {
    let (_d, db) = open();
    for (i, at) in [(1i64, NOW - DAY), (2, NOW - DAY + 3_600_000), (3, NOW)] {
        db.insert(&req(i, at)).unwrap();
    }
    let y = db.summary(NOW - DAY - 1, NOW - DAY + DAY / 2).unwrap();
    assert_eq!(y.requests, 2, "昨天的条数不对");
    assert_eq!(y.cost_micros_exact, 6_000, "昨天的花费不对");
    let all = db.summary(0, NOW + 1).unwrap();
    assert!(
        y.cost_micros_exact < all.cost_micros_exact,
        "今天那条混进来了"
    );
}

/// 验收二：**哪家 TTFT 最差。**
#[test]
fn two_you_can_ask_which_upstream_has_the_worst_ttft() {
    let (_d, db) = open();
    for (i, p, ttfb) in [
        (1i64, "官方", 300i64),
        (2, "官方", 320),
        (3, "中转", 2500),
        (4, "中转", 2600),
    ] {
        let mut r = req(i, NOW);
        r.provider = p.into();
        r.ttfb_ms = Some(ttfb);
        db.insert(&r).unwrap();
    }
    let mut by = db.latency_by_provider(0, NOW + 1).unwrap();
    by.sort_by_key(|l| -l.p50);
    assert_eq!(by[0].model, "中转", "最差的那家认错了：{by:?}");
    assert!(by[0].p50 > by[1].p50);
    // **样本数要一起给** —— 「2500ms」是 2 个样本还是 200 个，含义不同
    assert_eq!(by[0].samples, 2);
}

/// 验收三：**缓存省了多少。**
#[test]
fn three_you_can_ask_how_much_the_cache_saved() {
    let (_d, db) = open();
    for i in 1..=3 {
        db.insert(&req(i, NOW)).unwrap();
    }
    assert_eq!(db.summary(0, NOW + 1).unwrap().cache_saved_micros, 4_500);
}

/// 验收四：**混入订阅型上游之后金额不失真。**
///
/// 订阅制的边际成本是零，按 API 价目表给它算一个数字是纯虚构的。
/// 所以它**不进金额合计**，但要能数出来有多少条 ——
/// 「$1.23」和「$1.23，另有 4 条走的订阅」是两个不同的结论。
#[test]
fn four_a_subscription_upstream_does_not_distort_the_money() {
    let (_d, db) = open();
    for i in 1..=2 {
        db.insert(&req(i, NOW)).unwrap();
    }
    let before = db.summary(0, NOW + 1).unwrap();
    for i in 3..=6 {
        let mut r = req(i, NOW);
        r.provider = "订阅家".into();
        r.billing = "subscription".into();
        // 订阅制那几条没有成本 —— 它的账不在这个维度上
        r.cost_micros = None;
        db.insert(&r).unwrap();
    }
    let after = db.summary(0, NOW + 1).unwrap();
    assert_eq!(
        after.cost_micros_exact, before.cost_micros_exact,
        "**订阅制把金额算进去了**，那个数字是虚构的"
    );
    assert_eq!(after.requests, 6, "订阅制那几条要能被数出来");
    assert_eq!(after.subscription_requests, 4, "订阅制的条数没单独数");
    // 它们也不该被算成「算不出价钱」 —— 那是另一种状态（三态）
    assert_eq!(after.unpriced_requests, 0, "订阅制被当成「不知道价格」了");
}
