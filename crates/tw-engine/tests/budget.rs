//! 延迟目标：**入站解析 + 路由决策 < 2ms**。
//!
//! 这个数字从第一天就写在文档里，而在此之前**没有任何东西在守它** ——
//! 一个写在文档里、没人验的目标，和没有目标的区别只在于它让人以为验过。
//!
//! 这里量的是路由决策那一半（解析那一半跟着请求体大小走，在网关）。
//! **不是基准测试**：我们不关心它是 3µs 还是 6µs，只关心「有没有某次
//! 改动让它变成毫秒级」—— 比如在热路径上加了一次正则编译或配置深拷贝。

use std::time::{Duration, Instant};

use tw_engine::{Engine, RequestFacts, Rule};

/// 一份**比真实配置更重**的：20 个上游、31 条规则。
///
/// 按真实配置量的话，这条测试对「规则一多就慢」完全不敏感 —— 而那正是
/// 最可能发生的回归。
fn heavy() -> Engine {
    let providers: Vec<String> = (0..20).map(|i| format!("上游{i}")).collect();
    let route = |name: String, to: String, model: Option<String>| Rule {
        name,
        when: tw_engine::rule::When {
            model,
            ..Default::default()
        },
        to: Some(to),
        set: None,
        deny: None,
        guard: None,
    };
    let mut routes: Vec<Rule> = (0..30)
        .map(|i| {
            route(
                format!("规则{i}"),
                format!("上游{}", i % 20),
                Some(format!("模型{i}")),
            )
        })
        .collect();
    routes.push(route("其余".into(), "上游0".into(), None));
    Engine::with_default_rules(providers, Vec::new(), routes)
}

#[test]
fn a_routing_decision_stays_far_under_the_two_millisecond_budget() {
    let e = heavy();
    // **最坏情况**：命中兜底，也就是前面 30 条全都比过一遍
    let f = RequestFacts {
        model: "没人认识的模型".into(),
        client: "我".into(),
        dialect: "anthropic".into(),
        ..Default::default()
    };
    for _ in 0..100 {
        let _ = e.route(&f);
    }
    const N: u32 = 2_000;
    let t = Instant::now();
    for _ in 0..N {
        let _ = e.route(&f);
    }
    let per = t.elapsed() / N;
    // 目标 2ms。**门槛放在 200µs** —— 真实值该在几微秒，而放到 2ms
    // 等于这条测试只能抓住「慢了一千倍」那种灾难
    assert!(
        per < Duration::from_micros(200),
        "一次路由决策要 {per:?}，而目标是整条（解析 + 路由）不超过 2ms。\n\
         热路径上是不是加了正则编译、配置深拷贝、或者一次分配？"
    );
    println!("一次路由决策 {per:?}（20 个上游、31 条规则，命中兜底）");
}
