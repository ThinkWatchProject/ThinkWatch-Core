//! 把 `tests/fixtures/` 里的语料全部回放一遍。
//!
//! **这个测试和别的都不一样**：它不验证某段代码的行为，它验证的是
//! 「我们对真实流量的理解没有变」。上游漂移和我们自己的回归，都会在
//! 这里表现为一条说清「录的是什么、现在是什么」的差异。

use std::path::Path;

use tw_gateway::fixture::{Fixture, replay};

fn load_all() -> Vec<(String, Fixture)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return out;
    };
    let mut paths: Vec<_> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "yaml"))
        .collect();
    // 顺序固定，否则失败信息每次都在换位置
    paths.sort();
    for p in paths {
        let text =
            std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("读不了 {}：{e}", p.display()));
        let f: Fixture = serde_yaml_ng::from_str(&text)
            .unwrap_or_else(|e| panic!("{} 不是一份合法的用例：{e}", p.display()));
        out.push((p.file_name().unwrap().to_string_lossy().to_string(), f));
    }
    out
}

#[test]
fn every_recorded_case_still_extracts_the_same_semantics() {
    let all = load_all();
    assert!(!all.is_empty(), "语料是空的 —— 这个测试就白跑了");
    let mut bad = Vec::new();
    for (name, f) in &all {
        for d in replay(f) {
            bad.push(format!("{name}（{}）：{d}", f.name));
        }
    }
    assert!(bad.is_empty(), "回放对不上：\n  {}", bad.join("\n  "));
    println!("回放了 {} 个用例", all.len());
}

#[test]
fn no_fixture_carries_a_credential() {
    // **夹具会进 git**，一个装满真实密钥的目录被 push 上去就再也收不
    // 回来了。导出时脱过一遍，这里再守一道 —— 手写或者手改过
    // 的用例不走导出那条路。
    for (name, f) in load_all() {
        let dump = serde_yaml_ng::to_string(&f).unwrap();
        // 内置的凭据规则全开，内网地址不算（夹具里的 10.x 是示意）
        let all: Vec<&str> = tw_guard::redact::rules::BUILTINS
            .iter()
            .map(|b| b.id)
            .collect();
        let hits =
            tw_guard::redact::rules::scan(&dump, &tw_guard::redact::rules::RuleSet::only(&all));
        let real: Vec<_> = hits
            .iter()
            .filter(|h| h.rule.kind() != tw_guard::redact::rules::Kind::Internal)
            .map(|h| format!("{}（{}）", h.rule.id(), &dump[h.bytes.clone()]))
            .collect();
        assert!(real.is_empty(), "{name} 里有凭据：{real:?}");
    }
}

#[test]
fn every_fixture_says_where_it_came_from() {
    // **「实录」和「按文档构造」的可信度差一个量级。**一个没有来历的
    // 用例，出问题时没人知道该信它还是信代码。
    for (name, f) in load_all() {
        assert!(!f.note.trim().is_empty(), "{name} 没写来历");
        assert!(!f.name.trim().is_empty(), "{name} 没写名字");
    }
}
