//! 内容规则查、删、再查的每一步都还是线性的，套几十层也一样。
//!
//! 曾经不是：安全日志要每一处的位置（[`places`]），删过几遍之后冒出来的那几处要一遍一遍地
//! 换回原文、每一遍在那一遍的改动里一条条地找，记下来之前还要在记过的里面一条条地比 ——
//! 一个套了 64 层、128 KiB 的请求要 3 秒，1 MiB 的要 3 分钟，而它是在转发之前同步做的。
//! 整份的回答删了几万处之后，每一处接缝附近再找一次时一路找到了末尾。
//!
//! **不卡严格的时间**：同一件事做两种大小、交替各跑几遍取中位数，看两者之比。线性的是
//! 8 倍上下，平方级的是 64 倍。

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tw_dialect::ir::Dialect;
use tw_guard::content::answer::Answer;
use tw_guard::content::{Action, Match, RuleInput, Rules, Sides, places};

fn strip(pattern: &'static str, request: bool) -> Rules {
    Rules::build([RuleInput {
        id: "r",
        name: "r",
        custom: true,
        pattern,
        matching: Match::Contains,
        action: Action::Strip,
        sides: Sides {
            request,
            response: !request,
        },
        ..RuleInput::default()
    }])
    .unwrap()
}

/// 八倍大小花的时间是一倍大小的几倍：线性的 8 倍上下，平方级的 64 倍。两种大小**交替**各跑
/// 五遍、各取中位数（并行跑的测试抢 CPU 时，一阵忙落在哪一遍上都不至于左右结论），留足余量，
/// 只挡平方级
fn linear(what: &str, small: impl Fn() -> Duration, large: impl Fn() -> Duration) {
    let (mut s, mut l) = (Vec::new(), Vec::new());
    for _ in 0..5 {
        s.push(small());
        l.push(large());
    }
    s.sort_unstable();
    l.sort_unstable();
    let (s, l) = (s[2], l[2]);
    let ratio = l.as_secs_f64() / s.as_secs_f64().max(1e-6);
    assert!(
        ratio < 24.0,
        "{what}：八倍大小花了 {ratio:.1} 倍（{s:?} → {l:?}），不再是线性的"
    );
}

fn request_places(segment: &str) -> Duration {
    let rules = strip("ab", true);
    let body = json!({"messages": [
        {"role": "user", "content": segment},
        {"role": "user", "content": "keep me"},
    ]});
    let started = Instant::now();
    let (_, found) = places(&rules, Dialect::Anthropic, &body);
    let took = started.elapsed();
    assert_eq!(found.len(), 1);
    assert!(found[0].at.iter().all(|(s, _)| *s == 0));
    took
}

#[test]
fn places_in_a_request_nested_64_deep_cost_linear_time() {
    let nest = format!("{}{}", "a".repeat(64), "b".repeat(64));
    // 每一层都是一处：位置换回原文是外面那一对 `a…b` 两头之间
    let rules = strip("ab", true);
    let body: Value = json!({"messages": [{"role": "user", "content": nest.repeat(2)}]});
    let (_, found) = places(&rules, Dialect::Anthropic, &body);
    let at = &found[0].at;
    assert_eq!(at.len(), 128);
    assert!(at.contains(&(0, 0..128)) && at.contains(&(0, 128..256)));
    assert!(at.contains(&(0, 63..65)));
    linear(
        "套 64 层的请求上找每一处",
        || request_places(&nest.repeat(128)),
        || request_places(&nest.repeat(1024)),
    );
    linear(
        "`aabb` 连着几万遍的请求上找每一处",
        || request_places(&"aabb".repeat(12_500)),
        || request_places(&"aabb".repeat(100_000)),
    );
}

fn answer_whole(strips: usize) -> Duration {
    // 再加一条从来不命中的正则：找不到的时候也不能一路找到末尾
    let idle = RuleInput {
        id: "idle",
        name: "idle",
        custom: true,
        pattern: r"\d{3}-\d{4}-zz",
        matching: Match::Regex,
        action: Action::Strip,
        sides: Sides {
            request: false,
            response: true,
        },
        ..RuleInput::default()
    };
    let rules = Rules::build([
        RuleInput {
            id: "r",
            name: "r",
            custom: true,
            pattern: "secret",
            matching: Match::Contains,
            action: Action::Strip,
            sides: idle.sides,
            ..RuleInput::default()
        },
        idle,
    ])
    .unwrap();
    let a = Answer::build(&rules).unwrap();
    let text: String = (0..strips)
        .map(|_| format!("{} secret ", "x".repeat(50)))
        .collect();
    let started = Instant::now();
    let out = a.lane(true).whole(&text);
    let took = started.elapsed();
    assert!(!out.contains("secret"));
    took
}

#[test]
fn a_whole_answer_with_many_strips_is_checked_again_in_linear_time() {
    linear(
        "整份回答删了几万处之后接缝附近再找",
        || answer_whole(2_500),
        || answer_whole(20_000),
    );
}
