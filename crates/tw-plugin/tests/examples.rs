//! 仓库根目录 `examples/plugins/` 里的示例插件：每一个都加载得了，并且在样例
//! 输入上做了它头注释里说的事。示例是给人照着写的，跑不通的示例比没有示例更糟。

mod common;

use std::path::PathBuf;

use common::*;
use serde_json::{Value, json};
use tw_plugin::{Permission, Plugin, ReplyMode, RequestOutcome, ToolCallOutcome};

fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins")
}

fn example(name: &str) -> Plugin {
    let path = examples_dir().join(format!("{name}.js"));
    let src = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    rt().load(&src)
        .unwrap_or_else(|e| panic!("examples/plugins/{name}.js failed to load: {e:?}"))
}

/// 插件页填设置时，没填的取 manifest 里的默认值；这里照做
fn defaults(p: &Plugin) -> Value {
    let mut m = serde_json::Map::new();
    for s in &p.manifest().settings {
        m.insert(s.key.clone(), s.default.clone());
    }
    Value::Object(m)
}

#[test]
fn every_example_loads_and_is_covered_here() {
    let covered = [
        "add-date",
        "mask-pattern",
        "strip-params",
        "unify-terms",
        "wsl-paths",
    ];
    let mut found: Vec<String> = std::fs::read_dir(examples_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".js"))
        .map(|n| n.trim_end_matches(".js").to_string())
        .collect();
    found.sort();
    assert_eq!(
        found, covered,
        "an example without a test, or a test without an example"
    );
    for name in covered {
        let p = example(name);
        let m = p.manifest();
        assert!(!m.name.is_empty() && m.description.is_some(), "{name}");
    }
}

#[test]
fn add_date_appends_today_in_the_configured_time_zone() {
    let p = example("add-date");
    assert_eq!(
        p.manifest().permissions,
        [Permission::System].into_iter().collect()
    );
    let view =
        json!({ "format": "anthropic", "model": "claude-sonnet-4-5", "system": "你是助手。" });
    let inv = p.on_request(view, ctx(defaults(&p)));
    let system = match inv.result {
        Ok(RequestOutcome::Changed(v)) => v["system"].as_str().unwrap().to_string(),
        other => panic!("{other:?}"),
    };
    let date = system
        .strip_prefix("你是助手。\n\n今天的日期：")
        .unwrap_or_else(|| panic!("{system}"));
    // 默认是北京时间：和宿主按 UTC+8 算出的今天一致（跨零点的那一瞬间允许差一天）
    let now = chrono::Utc::now() + chrono::Duration::hours(8);
    let today = now.format("%Y-%m-%d").to_string();
    let yesterday = (now - chrono::Duration::minutes(1))
        .format("%Y-%m-%d")
        .to_string();
    assert!(date == today || date == yesterday, "{date} vs {today}");

    // 没有系统提示词时，日期就是整个系统提示词
    let view = json!({ "format": "openai_chat", "model": "gpt-5", "system": "" });
    match p.on_request(view, ctx(defaults(&p))).result {
        Ok(RequestOutcome::Changed(v)) => {
            assert!(
                v["system"].as_str().unwrap().starts_with("今天的日期："),
                "{v}"
            )
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn unify_terms_replaces_terms_split_across_streamed_pieces() {
    let p = example("unify-terms");
    assert_eq!(p.manifest().reply_mode, ReplyMode::Stream);
    let mut r = p.reply(reply_ctx(defaults(&p))).unwrap();
    let mut out = String::new();
    for piece in ["请先登", "陆你的帐", "号，再登陆", "。帐"] {
        out.push_str(
            &r.on_text(piece)
                .result
                .unwrap()
                .unwrap_or_else(|| piece.into()),
        );
    }
    out.push_str(&r.on_text_end().result.unwrap().unwrap_or_default());
    assert_eq!(out, "请先登录你的账号，再登录。帐");

    // 扣住的只有可能是原词开头的那几个字
    let mut r = p.reply(reply_ctx(defaults(&p))).unwrap();
    assert_eq!(r.on_text("请先登").result.unwrap().as_deref(), Some("请先"));
}

#[test]
fn strip_params_removes_the_named_parameters_and_leaves_others_alone() {
    let p = example("strip-params");
    let view = json!({
        "format": "anthropic",
        "model": "claude-sonnet-4-5",
        "params": { "model": "claude-sonnet-4-5", "max_tokens": 1024, "temperature": 0.7, "top_p": 0.9 }
    });
    match p
        .on_request(view, ctx(json!({ "names": "top_p, max_tokens" })))
        .result
    {
        Ok(RequestOutcome::Changed(v)) => assert_eq!(
            v["params"],
            json!({ "model": "claude-sonnet-4-5", "max_tokens": 1024, "temperature": 0.7 })
        ),
        other => panic!("{other:?}"),
    }
    // 没有要删的参数：原样不动
    let view = json!({
        "format": "anthropic",
        "model": "claude-sonnet-4-5",
        "params": { "model": "claude-sonnet-4-5", "max_tokens": 1024 }
    });
    assert!(matches!(
        p.on_request(view, ctx(defaults(&p))).result,
        Ok(RequestOutcome::Unchanged)
    ));
}

#[test]
fn mask_pattern_replaces_what_matches_and_keeps_dollar_signs_literal() {
    let p = example("mask-pattern");
    let mut r = p.reply(reply_ctx(defaults(&p))).unwrap();
    assert_eq!(
        r.on_text("连到 build-01.corp.example.com 和 DB.corp.example.com，再看 example.com")
            .result
            .unwrap()
            .as_deref(),
        Some("连到 [内部地址] 和 [内部地址]，再看 example.com")
    );
    let mut r = p
        .reply(reply_ctx(
            json!({ "pattern": "EMP-\\d{6}", "replacement": "$&" }),
        ))
        .unwrap();
    assert_eq!(
        r.on_text("员工 EMP-123456").result.unwrap().as_deref(),
        Some("员工 $&")
    );
}

#[test]
fn wsl_paths_rewrites_whole_path_arguments_only() {
    let p = example("wsl-paths");
    assert!(
        p.manifest()
            .permissions
            .contains(&Permission::ReplyToolCalls)
    );
    let mut r = p.reply(reply_ctx(defaults(&p))).unwrap();
    let call = json!({
        "id": "toolu_1",
        "name": "Read",
        "input": { "file_path": "/mnt/c/Users/me/a b.txt", "command": "cat /mnt/c/x", "n": 1 }
    });
    match r.on_tool_call(call).result {
        Ok(ToolCallOutcome::Replace(calls)) => assert_eq!(
            calls,
            vec![json!({
                "id": "toolu_1",
                "name": "Read",
                "input": { "file_path": "C:\\Users\\me\\a b.txt", "command": "cat /mnt/c/x", "n": 1 }
            })]
        ),
        other => panic!("{other:?}"),
    }
    // 另一个方向
    let mut r = p.reply(reply_ctx(json!({ "to": "wsl" }))).unwrap();
    match r
        .on_tool_call(json!({ "id": "t", "name": "Write", "input": { "path": "D:\\work\\x.rs" } }))
        .result
    {
        Ok(ToolCallOutcome::Replace(calls)) => {
            assert_eq!(calls[0]["input"]["path"], "/mnt/d/work/x.rs")
        }
        other => panic!("{other:?}"),
    }
    // 没有路径的调用原样放过
    assert!(matches!(
        r.on_tool_call(json!({ "id": "t2", "name": "Bash", "input": { "command": "ls" } }))
            .result,
        Ok(ToolCallOutcome::Unchanged)
    ));
}
