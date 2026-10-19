//! 恢复：按项目列差异、挑几项恢复、本机设置和登录留着、固定密钥不删。

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::model::WholeFile;
use crate::{Manifest, PackKind, PackOrigin};

const KEY: &str = "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00";
const OLD_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn pack(text: &str) -> Pack {
    Pack {
        manifest: Manifest::new(PackKind::Full, PackOrigin::Ui),
        config_yaml: text.to_string(),
        plugins: BTreeMap::new(),
        client_prefs: None,
    }
}

fn now() -> Pack {
    pack(&format!(
        "version: 1\nlisten:\n  control:\n    key: {KEY}\n  gateway:\n    port: 9000\nclients:\n  - name: default\n    key: tw-a\n  - name: claude-code\n    key: tw-cc\n    client: claude-code\nproviders:\n  - name: kept\n    base_url: https://kept.example\n  - name: chatgpt\n    base_url: https://chatgpt.com/backend-api/codex\n    oauth:\n      refresh: rt-now\n      endpoint: https://auth.openai.com/oauth/token\n"
    ))
}

/// 一个旧版本：密钥 claude-code 那时还没有，有一个后来删掉的上游，chatgpt 的 token 是旧的
fn then() -> Pack {
    pack(&format!(
        "version: 1\nlisten:\n  control:\n    key: {}\n  gateway:\n    port: 8000\nclients:\n  - name: default\n    key: tw-old\nproviders:\n  - name: kept\n    base_url: https://kept.example/v0\n  - name: deleted\n    base_url: https://deleted.example\n  - name: chatgpt\n    base_url: https://chatgpt.com/backend-api/codex\n    oauth:\n      refresh: rt-then\n      endpoint: https://auth.openai.com/oauth/token\n  - name: old-gpt\n    base_url: https://chatgpt.com/backend-api/codex\n    oauth:\n      refresh: rt-gone\n      endpoint: https://auth.openai.com/oauth/token\n",
        tw_api::control::KEY_MASK
    ))
}

fn pinned() -> BTreeSet<String> {
    ["claude-code".to_string()].into_iter().collect()
}

#[test]
fn the_preview_lists_what_differs_by_item() {
    let rows = preview_with(&now(), &then(), &pinned(), true, &WholeFile).unwrap();
    let got: Vec<(String, Change, Option<Kept>)> = rows
        .iter()
        .map(|r| (r.item.to_string(), r.change, r.kept))
        .collect();
    assert_eq!(
        got,
        vec![
            ("provider `deleted`".into(), Change::Add, None),
            ("provider `kept`".into(), Change::Modify, None),
            ("provider `old-gpt`".into(), Change::Add, None),
            (
                "key `claude-code`".into(),
                Change::Remove,
                Some(Kept::KeyInUse)
            ),
            ("key `default`".into(), Change::Modify, None),
            ("provider_order".into(), Change::Modify, None),
        ]
    );
}

#[test]
fn restoring_everything_keeps_local_settings_logins_and_keys_in_use() {
    let out = apply_with(&now(), &then(), None, &pinned(), true, &WholeFile).unwrap();
    let cfg = tw_config::try_parse(&out.pack.config_yaml)
        .unwrap_or_else(|e| panic!("{e}\n{}", out.pack.config_yaml));
    assert_eq!(cfg.listen.gateway.port, 9000, "本机设置留着现在的");
    assert!(out.pack.config_yaml.contains(KEY));
    let p = |n: &str| cfg.providers.iter().find(|p| p.name == n);
    assert_eq!(p("kept").unwrap().base_url, "https://kept.example/v0");
    assert!(p("deleted").is_some());
    // 登录是现在的；回来的那一家停用着，旧 token 不写回
    assert_eq!(
        p("chatgpt").unwrap().oauth.as_ref().unwrap().refresh,
        "rt-now"
    );
    let old = p("old-gpt").unwrap();
    assert!(old.disabled && old.oauth.is_none());
    assert!(!out.pack.config_yaml.contains("rt-gone"));
    // 接管在用的密钥没删
    assert!(cfg.clients.iter().any(|c| c.name == "claude-code"));
    assert_eq!(out.kept, [ItemId::new(ItemKind::Key, "claude-code")]);
    assert_eq!(out.changed_keys, ["default"]);
}

#[test]
fn restoring_a_few_items_leaves_the_rest_as_it_is() {
    let pick = [ItemId::new(ItemKind::Provider, "deleted")];
    let out = apply_with(&now(), &then(), Some(&pick), &pinned(), true, &WholeFile).unwrap();
    let cfg = tw_config::try_parse(&out.pack.config_yaml).unwrap();
    let names: Vec<&str> = cfg.providers.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["kept", "chatgpt", "deleted"]);
    assert_eq!(cfg.providers[0].base_url, "https://kept.example");
    assert!(out.changed_keys.is_empty());
}

#[test]
fn in_safe_mode_the_whole_version_comes_back_with_todays_key() {
    let broken = format!("version: 1\nlisten:\n  control:\n    key: {OLD_KEY}\nproviders: [\n");
    let out = whole(&then(), &broken);
    assert!(out.config_yaml.contains(OLD_KEY), "{}", out.config_yaml);
    assert!(!out.config_yaml.contains(tw_api::control::KEY_MASK));
    assert!(out.config_yaml.contains("port: 8000"));
}
