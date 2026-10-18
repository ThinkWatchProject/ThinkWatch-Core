//! 拆开、落回、改名。表驱动：每一种项目的每一种改法，落回去再拆开都是要的那个样子。

use std::collections::BTreeMap;

use serde_json::{Value as Json, json};

use super::*;
use crate::{Manifest, Pack, PackKind, PackOrigin, PluginFiles};

const KEY: &str = "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00";

/// 一份什么都有的配置。注释散在各处：没改的项目落回去之后它们还在
fn rich() -> String {
    format!(
        r#"version: 1
listen:
  control:
    key: {KEY}
  gateway:
    port: 9000
retention:
  body_days: 3
clients:
  - name: default
    key: tw-aaa
  # 给 Codex 的
  - name: codex
    key: tw-bbb
    route: 长上下文
    client: codex
    allow: [fast]
proxies:
  - name: hk
    type: socks5h
    addr: 127.0.0.1:7890
providers:
  # 官方直连
  - name: 官方
    base_url: https://api.anthropic.com  # 不走代理
    key: sk-a
    proxy: direct
    headers:
      X-Second: b
      X-First: a
  - name: relay
    base_url: https://relay.example/v1
    key: sk-b
    proxy: hk
    pricing: 中转
    models: [m1]
    max_concurrent: 4
  - name: chatgpt
    base_url: https://chatgpt.com/backend-api/codex
    oauth:
      refresh: rt-1
      endpoint: https://auth.openai.com/oauth/token
      client_id: app_EMoamEEZ73f0CkXaXp7hrann
    headers:
      ChatGPT-Account-Id: acct-1
pricing:
  auto_update: false
  sheets:
    - name: 中转
      multiplier: 0.5
failover:
  pause_secs: 120
security:
  redact:
    mode: enforce
    custom:
      - name: 项目代号
        pattern: 北极星
  content:
    custom:
      - name: 脏话
        pattern: xx
aliases:
  fast: [claude-haiku, gpt-mini]
groups:
  - name: pool
    type: fallback
    providers: [官方, relay]
routes:
  - name: default
    rules:
      - name: 兜底
        to: pool
  - name: 长上下文
    rules:
      - name: big
        when: {{ input_tokens: '>200k', client: codex }}
        to: relay
default_key: default
"#
    )
}

fn pack(text: &str) -> Pack {
    Pack {
        manifest: Manifest::new(PackKind::Full, PackOrigin::Ui),
        config_yaml: text.to_string(),
        plugins: BTreeMap::new(),
        client_prefs: None,
    }
}

fn id(kind: ItemKind, name: &str) -> ItemId {
    ItemId::new(kind, name)
}

/// 插件源码的测试拆法：第一行是 `// data: <JSON>`，其余是代码。名字带 `steer` 的要批准
struct Fake {
    shipped: BTreeMap<String, String>,
}

impl Fake {
    fn new() -> Self {
        Self {
            shipped: BTreeMap::from([(
                "reply-language".to_string(),
                "// data: {\"lang\":\"\"}\nshipped code v1\n".to_string(),
            )]),
        }
    }
}

impl PluginCodec for Fake {
    fn split(&self, source: &str) -> Option<(String, Json)> {
        let (first, rest) = source.split_once('\n')?;
        let data: Json = serde_json::from_str(first.strip_prefix("// data: ")?).ok()?;
        Some((format!("// data: null\n{rest}"), data))
    }
    fn join(&self, source: &str, data: &Json) -> Option<String> {
        let (_, rest) = source.split_once('\n')?;
        Some(format!("// data: {data}\n{rest}"))
    }
    fn shipped(&self, id: &str) -> Option<String> {
        self.shipped.get(id).cloned()
    }
    fn needs_approval(&self, source: &str) -> bool {
        source.contains("steer")
    }
}

fn snap(p: &Pack) -> Snapshot {
    snapshot_with(p, &Fake::new()).unwrap_or_else(|e| panic!("{e}\n{}", p.config_yaml))
}

fn mat(local: &Pack, target: &Snapshot) -> Pack {
    materialize_with(local, target, &Fake::new()).unwrap_or_else(|e| panic!("{e}"))
}

fn without_orders(s: &Snapshot) -> Snapshot {
    let mut s = s.clone();
    s.items.retain(|id, _| !id.kind.is_order());
    s
}

// ─────────────────────────────────────────────────────────── 拆

#[test]
fn every_named_thing_is_an_item_and_local_settings_are_not() {
    let s = snap(&pack(&rich()));
    let ids: Vec<String> = s.items.keys().map(|i| i.to_string()).collect();
    for want in [
        "provider `官方`",
        "provider `relay`",
        "provider `chatgpt`",
        "proxy `hk`",
        "key `default`",
        "key `codex`",
        "alias `fast`",
        "group `pool`",
        "route `default`",
        "route `长上下文`",
        "price_sheet `中转`",
        "security_rule `redact/项目代号`",
        "security_rule `content/脏话`",
        "provider_order",
        "route_order",
        "global `failover`",
        "global `security`",
        "global `pricing`",
    ] {
        assert!(ids.contains(&want.to_string()), "{want} 不在：{ids:?}");
    }
    // 写成默认值的那一节不算（default_key: default 就是没写）
    assert!(!ids.iter().any(|i| i.contains("default_key")), "{ids:?}");
    let all = serde_json::to_string(&s).unwrap();
    for local in [KEY, "9000", "body_days", "rt-1", "acct-1"] {
        assert!(!all.contains(local), "本机设置进了快照：{local}");
    }
}

#[test]
fn an_upstream_splits_into_four_units_by_what_its_fields_are_about() {
    let s = snap(&pack(&rich()));
    let relay = &s.items[&id(ItemKind::Provider, "relay")];
    assert_eq!(
        relay.units.keys().collect::<Vec<_>>(),
        ["billing", "conn", "models", "other"]
    );
    assert_eq!(
        relay.units["conn"],
        json!({"base_url": "https://relay.example/v1", "key": "sk-b", "proxy": "hk"})
    );
    assert_eq!(relay.units["models"], json!({"models": ["m1"]}));
    assert_eq!(relay.units["billing"], json!({"pricing": "中转"}));
    assert_eq!(relay.units["other"], json!({"max_concurrent": 4}));
    // 写明的默认值（proxy: direct）和没写一样；请求头按写的顺序
    let official = &s.items[&id(ItemKind::Provider, "官方")];
    assert_eq!(
        official.units["conn"],
        json!({"base_url": "https://api.anthropic.com", "key": "sk-a",
               "headers": [["X-Second", "b"], ["X-First", "a"]]})
    );
    // ChatGPT 的登录和账户 ID 请求头不在里面
    let chatgpt = &s.items[&id(ItemKind::Provider, "chatgpt")];
    assert_eq!(
        chatgpt.units["conn"],
        json!({"base_url": "https://chatgpt.com/backend-api/codex"})
    );
}

#[test]
fn references_point_at_what_each_item_uses() {
    let s = snap(&pack(&rich()));
    let refs = |kind, name: &str| -> Vec<String> {
        s.items[&id(kind, name)]
            .refs
            .iter()
            .map(|r| r.to_string())
            .collect()
    };
    assert_eq!(
        refs(ItemKind::Provider, "relay"),
        ["proxy `hk`", "price_sheet `中转`"]
    );
    assert_eq!(
        refs(ItemKind::Key, "codex"),
        ["alias `fast`", "route `长上下文`"]
    );
    assert_eq!(
        refs(ItemKind::Group, "pool"),
        ["provider `relay`", "provider `官方`"]
    );
    assert_eq!(refs(ItemKind::Route, "default"), ["group `pool`"]);
    assert_eq!(
        refs(ItemKind::Route, "长上下文"),
        ["provider `relay`", "key `codex`"]
    );
    // 选一条路由导出：带上它的上游、上游的代理和价目表、按它分流的密钥
    let picked: Vec<String> = s
        .closure(&[id(ItemKind::Route, "长上下文")])
        .iter()
        .map(|i| i.to_string())
        .collect();
    assert_eq!(
        picked,
        [
            "provider `relay`",
            "proxy `hk`",
            "key `codex`",
            "alias `fast`",
            "route `长上下文`",
            "price_sheet `中转`"
        ]
    );
}

/// 注释、排版、写没写默认值都不是差别；本机设置也不是
#[test]
fn the_same_content_written_differently_is_the_same_snapshot_and_id() {
    let a = pack(&rich());
    let mut b = pack(
        &rich()
            .replace("  # 官方直连\n", "")
            .replace("    proxy: direct\n", "")
            .replace("  gateway:\n    port: 9000\n", "")
            .replace("retention:\n  body_days: 3\n", "")
            .replace("default_key: default\n", "")
            .replace("rt-1", "rt-2-after-a-rotation"),
    );
    assert_eq!(snap(&a), snap(&b));
    assert_eq!(a.content_id(), b.content_id());
    assert_eq!(a.content_id().len(), 12);
    b.config_yaml = b.config_yaml.replace("sk-b", "sk-changed");
    assert_ne!(a.content_id(), b.content_id());
}

#[test]
fn global_settings_hold_only_what_differs_from_the_default() {
    let s = snap(&pack(&rich()));
    assert_eq!(
        s.items[&id(ItemKind::Global, "failover")].units,
        BTreeMap::from([("failover.pause_secs".to_string(), json!(120))])
    );
    assert_eq!(
        s.items[&id(ItemKind::Global, "security")].units,
        BTreeMap::from([("security.redact.mode".to_string(), json!("enforce"))])
    );
    assert_eq!(
        s.items[&id(ItemKind::Global, "pricing")].units,
        BTreeMap::from([("pricing.auto_update".to_string(), json!(false))])
    );
    for (path, v, default) in [
        ("failover.pause_secs", json!(60), true),
        ("failover.pause_secs", json!(61), false),
        ("security.redact.mode", json!("observe"), true),
        ("security.content.mode", json!("off"), false),
        ("client_probes.titling", json!("intercept"), false),
        ("pricing.auto_update", json!(true), true),
        ("default_route", json!("default"), true),
        ("default_route", json!("other"), false),
        ("default_key", json!("default"), true),
    ] {
        assert_eq!(items::is_global_default(path, &v), default, "{path} = {v}");
    }
}

#[test]
fn a_pack_from_a_newer_version_is_too_new() {
    for text in [
        "version: 1\nsomething_new: true\n",
        "version: 1\nproviders:\n  - { name: a, base_url: https://a, balance: unheard_of }\n",
    ] {
        let e = snapshot_with(&pack(text), &Fake::new()).unwrap_err();
        assert!(e.is_too_new(), "{text}: {e:?}");
    }
    let e = snapshot_with(&pack("version: 99\n"), &Fake::new()).unwrap_err();
    assert!(e.is_too_new());
    assert_eq!(e.msg().code, "pack.schema_too_new");
    // 读不成 YAML 的不算太新
    let e = snapshot_with(&pack("version: 1\nproviders: [\n"), &Fake::new()).unwrap_err();
    assert!(!e.is_too_new());
    // 只有一个上游的部分包：没有网关密钥、没有钥匙，照样拆得开
    snap(&pack(
        "version: 1\nproviders:\n  - { name: a, base_url: https://a.example }\n",
    ));
    // 同步那一节这里不必认得
    snap(&pack(
        "version: 1\nsync:\n  device_id: x\n  whatever: [1]\n",
    ));
}

#[test]
fn a_snapshot_survives_json() {
    let s = snap(&pack(&rich()));
    let back: Snapshot = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    assert_eq!(back, s);
}

// ─────────────────────────────────────────────────────────── 落回

/// 原样落回：一个字节都不变
#[test]
fn materializing_a_config_onto_itself_changes_nothing() {
    let p = pack(&rich());
    let out = mat(&p, &snap(&p));
    assert_eq!(out.config_yaml, p.config_yaml);
}

/// 改一处：别的原样，注释也在；本机设置和 ChatGPT 的登录还在
#[test]
fn changing_one_item_leaves_the_others_byte_for_byte() {
    let p = pack(&rich());
    let mut t = snap(&p);
    t.items
        .get_mut(&id(ItemKind::Provider, "relay"))
        .unwrap()
        .units
        .insert(
            "other".into(),
            json!({"max_concurrent": 8, "disabled": true}),
        );
    let out = mat(&p, &t);
    assert!(
        out.config_yaml.contains("    max_concurrent: 8\n"),
        "{}",
        out.config_yaml
    );
    for kept in [
        "  # 官方直连\n",
        "base_url: https://api.anthropic.com  # 不走代理\n",
        "  # 给 Codex 的\n",
        "    port: 9000\n",
        "  body_days: 3\n",
        "      refresh: rt-1\n",
        "      ChatGPT-Account-Id: acct-1\n",
        "    proxy: direct\n",
    ] {
        assert!(
            out.config_yaml.contains(kept),
            "{kept:?} 没了：\n{}",
            out.config_yaml
        );
    }
    assert_eq!(normalized(&snap(&out)), normalized(&t));
    tw_config::try_parse(&out.config_yaml).unwrap_or_else(|e| panic!("{e}\n{}", out.config_yaml));
}

/// 一种项目一张表：加、删、改，落回去再拆开就是要的样子，配置照样过得了校验
#[test]
fn every_kind_of_change_lands_as_intended() {
    type Change = fn(&mut Snapshot);
    let cases: Vec<(&str, Change)> = vec![
        ("add an upstream", |s| {
            s.items.insert(
                ItemId::new(ItemKind::Provider, "new"),
                Item {
                    units: BTreeMap::from([
                        (
                            "conn".into(),
                            json!({"base_url": "https://new.example", "key": "sk-n"}),
                        ),
                        ("models".into(), json!({})),
                        ("billing".into(), json!({"billing": "free"})),
                        ("other".into(), json!({})),
                    ]),
                    ..Default::default()
                },
            );
        }),
        ("drop an upstream nobody uses", |s| {
            s.items.remove(&ItemId::new(ItemKind::Provider, "chatgpt"));
        }),
        ("change an upstream's headers and order", |s| {
            let p = s
                .items
                .get_mut(&ItemId::new(ItemKind::Provider, "官方"))
                .unwrap();
            p.units.insert(
                "conn".into(),
                json!({"base_url": "https://api.anthropic.com", "key": "sk-new",
                       "headers": [["X-First", "a"], ["X-Third", "c"]]}),
            );
        }),
        ("add a proxy", |s| {
            s.items.insert(
                ItemId::new(ItemKind::Proxy, "sg"),
                Item {
                    units: BTreeMap::from([(
                        "all".into(),
                        json!({"type": "http", "addr": "10.0.0.1:8080"}),
                    )]),
                    ..Default::default()
                },
            );
        }),
        ("change a key", |s| {
            s.items
                .get_mut(&ItemId::new(ItemKind::Key, "default"))
                .unwrap()
                .units
                .insert("all".into(), json!({"key": "tw-zzz", "max_concurrent": 2}));
        }),
        ("drop an alias and its use", |s| {
            s.items.remove(&ItemId::new(ItemKind::Alias, "fast"));
            s.items
                .get_mut(&ItemId::new(ItemKind::Key, "codex"))
                .unwrap()
                .units
                .insert(
                    "all".into(),
                    json!({"key": "tw-bbb", "route": "长上下文", "client": "codex"}),
                );
        }),
        ("add an alias", |s| {
            s.items.insert(
                ItemId::new(ItemKind::Alias, "big"),
                Item {
                    units: BTreeMap::from([("all".into(), json!(["claude-opus"]))]),
                    ..Default::default()
                },
            );
        }),
        ("change a group", |s| {
            s.items
                .get_mut(&ItemId::new(ItemKind::Group, "pool"))
                .unwrap()
                .units
                .insert(
                    "all".into(),
                    json!({"type": "load-balance", "providers": ["官方", {"name": "relay", "weight": 3}]}),
                );
        }),
        ("add a route and reorder", |s| {
            s.items.insert(
                ItemId::new(ItemKind::Route, "a-first"),
                Item {
                    units: BTreeMap::from([(
                        "all".into(),
                        json!({"rules": [{"name": "all", "to": "官方"}]}),
                    )]),
                    ..Default::default()
                },
            );
            s.items.insert(
                ItemId::new(ItemKind::RouteOrder, ""),
                Item {
                    units: BTreeMap::from([(
                        "all".into(),
                        json!(["长上下文", "a-first", "default"]),
                    )]),
                    ..Default::default()
                },
            );
        }),
        ("reorder upstreams", |s| {
            s.items.insert(
                ItemId::new(ItemKind::ProviderOrder, ""),
                Item {
                    units: BTreeMap::from([("all".into(), json!(["chatgpt", "relay", "官方"]))]),
                    ..Default::default()
                },
            );
        }),
        ("change a price sheet", |s| {
            s.items
                .get_mut(&ItemId::new(ItemKind::PriceSheet, "中转"))
                .unwrap()
                .units
                .insert("all".into(), json!({"multiplier": 0.8}));
        }),
        ("add and drop custom rules", |s| {
            s.items
                .remove(&ItemId::new(ItemKind::SecurityRule, "content/脏话"));
            s.items.insert(
                ItemId::new(ItemKind::SecurityRule, "inspect_tools/rm"),
                Item {
                    units: BTreeMap::from([("all".into(), json!({"pattern": "rm -rf"}))]),
                    ..Default::default()
                },
            );
        }),
        ("change and reset global fields", |s| {
            let f = s
                .items
                .get_mut(&ItemId::new(ItemKind::Global, "failover"))
                .unwrap();
            f.units.clear();
            f.units
                .insert("failover.idle_timeout_secs".into(), json!(600));
            s.items.remove(&ItemId::new(ItemKind::Global, "pricing"));
            s.items.insert(
                ItemId::new(ItemKind::Global, "security"),
                Item {
                    units: BTreeMap::from([
                        ("security.content.mode".into(), json!("enforce")),
                        ("security.redact.actions.jwt".into(), json!("record")),
                    ]),
                    ..Default::default()
                },
            );
            s.items.insert(
                ItemId::new(ItemKind::Global, "client_probes"),
                Item {
                    units: BTreeMap::from([("client_probes.titling".into(), json!("forward"))]),
                    ..Default::default()
                },
            );
        }),
        ("set the default route and key", |s| {
            s.items.insert(
                ItemId::new(ItemKind::Global, "default_route"),
                Item {
                    units: BTreeMap::from([("default_route".into(), json!("长上下文"))]),
                    ..Default::default()
                },
            );
            s.items.insert(
                ItemId::new(ItemKind::Global, "default_key"),
                Item {
                    units: BTreeMap::from([("default_key".into(), json!("codex"))]),
                    ..Default::default()
                },
            );
        }),
        ("an explicit default is the same as none", |s| {
            s.items.insert(
                ItemId::new(ItemKind::Global, "client_probes"),
                Item {
                    units: BTreeMap::from([("client_probes.warmup".into(), json!("intercept"))]),
                    ..Default::default()
                },
            );
        }),
    ];
    for (what, change) in cases {
        let p = pack(&rich());
        let mut t = snap(&p);
        change(&mut t);
        t.relink();
        let out = materialize_with(&p, &t, &Fake::new()).unwrap_or_else(|e| panic!("{what}: {e}"));
        let got = normalized(&snap(&out));
        assert_eq!(
            without_orders(&got),
            without_orders(&normalized(&t)),
            "{what}:\n{}",
            out.config_yaml
        );
        // 目标里列着的顺序照着排；没列的（新加的上游）接在后面
        for (oid, o) in t.items.iter().filter(|(i, _)| i.kind.is_order()) {
            let have = got.items[oid].units["all"].as_array().unwrap();
            let want: Vec<&Json> = o.units["all"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|n| have.contains(n))
                .collect();
            let have: Vec<&Json> = have.iter().take(want.len()).collect();
            assert_eq!(have, want, "{what}: {oid}");
        }
        tw_config::try_parse(&out.config_yaml)
            .unwrap_or_else(|e| panic!("{what}: {e}\n{}", out.config_yaml));
        // 本机设置都还在；ChatGPT 那一家还在的话，它的登录也在
        // 注释也在：走的是按项目改原文那条路，不是整份重写
        let mut kept = vec![
            "    port: 9000\n",
            "  body_days: 3\n",
            KEY,
            "  # 给 Codex 的\n",
            "  # 官方直连\n",
        ];
        if t.items.contains_key(&id(ItemKind::Provider, "chatgpt")) {
            kept.push("      refresh: rt-1\n");
        }
        for kept in kept {
            assert!(
                out.config_yaml.contains(kept),
                "{what}: {kept:?} 没了\n{}",
                out.config_yaml
            );
        }
    }
}

/// 按项目改不动的写法（行内的一节删不了键）：整份重写，内容照样对
#[test]
fn a_shape_the_edits_cannot_touch_is_rewritten_whole() {
    let text = format!(
        "version: 1\nlisten: {{ control: {{ key: {KEY} }} }}\nclients: [{{ name: default, key: tw-a }}]\nfailover: {{ pause_secs: 120, idle_timeout_secs: 600 }}\n"
    );
    let p = pack(&text);
    let mut t = snap(&p);
    t.items
        .get_mut(&id(ItemKind::Global, "failover"))
        .unwrap()
        .units
        .remove("failover.idle_timeout_secs");
    let out = mat(&p, &t);
    assert_eq!(
        normalized(&snap(&out)),
        normalized(&t),
        "{}",
        out.config_yaml
    );
    assert!(out.config_yaml.contains(KEY));
    tw_config::try_parse(&out.config_yaml).unwrap();
}

/// 从空的配置开始落：导出所选项目就是这么生成配置包的
#[test]
fn materializing_onto_an_empty_config_builds_just_those_items() {
    let full = snap(&pack(&rich()));
    let mut picked = full.clone();
    picked.retain(&full.closure(&[id(ItemKind::Route, "长上下文")]));
    let out = mat(&pack("version: 1\n"), &picked);
    assert_eq!(
        normalized(&snap(&out)),
        normalized(&picked),
        "{}",
        out.config_yaml
    );
    assert!(!out.config_yaml.contains("官方"), "{}", out.config_yaml);
}

/// ChatGPT 账号上游：落到别的设备上时没有登录；落回本机时留着本机的登录
#[test]
fn a_chatgpt_upstream_keeps_the_login_of_the_device_it_lands_on() {
    let p = pack(&rich());
    let mut t = snap(&p);
    t.items
        .get_mut(&id(ItemKind::Provider, "chatgpt"))
        .unwrap()
        .units
        .insert("other".into(), json!({"disabled": true}));
    let out = mat(&p, &t);
    let cfg = tw_config::try_parse(&out.config_yaml).unwrap();
    let c = cfg.providers.iter().find(|p| p.name == "chatgpt").unwrap();
    assert!(c.disabled);
    assert_eq!(c.oauth.as_ref().unwrap().refresh, "rt-1");
    assert_eq!(
        c.headers.get("ChatGPT-Account-Id").unwrap().value.raw(),
        "acct-1"
    );
    // 别的设备上：只有上游本身
    let other = mat(&pack("version: 1\n"), &t);
    assert!(!other.config_yaml.contains("rt-1"), "{}", other.config_yaml);
    assert!(
        !other.config_yaml.contains("acct-1"),
        "{}",
        other.config_yaml
    );
}

// ─────────────────────────────────────────────────────────── 插件

fn with_plugins(entries: &[(&str, &str, bool)]) -> Pack {
    let mut text = format!(
        "version: 1\nlisten:\n  control:\n    key: {KEY}\nclients:\n  - name: default\n    key: tw-a\nplugins:\n"
    );
    let mut plugins = BTreeMap::new();
    for (id, source, enabled) in entries {
        let sha = items::sha256_hex(source.as_bytes());
        text.push_str(&format!(
            "  - id: {id}\n    file: plugins/{id}.js\n    sha256: {sha}\n    enabled: {enabled}\n"
        ));
        plugins.insert(
            id.to_string(),
            PluginFiles {
                script: source.as_bytes().to_vec(),
                approved: Some(source.as_bytes().to_vec()),
            },
        );
    }
    Pack {
        plugins,
        ..pack(&text)
    }
}

#[test]
fn a_plugin_splits_into_code_settings_and_switch() {
    let p = with_plugins(&[
        ("note", "// data: {\"v\":1}\nmy code\n", true),
        (
            "reply-language",
            "// data: {\"lang\":\"zh\"}\nshipped code v1\n",
            false,
        ),
    ]);
    let s = snap(&p);
    let note = &s.items[&id(ItemKind::Plugin, "note")];
    assert_eq!(note.units["script"], json!("// data: null\nmy code\n"));
    assert_eq!(note.units["settings"], json!({"v": 1}));
    assert_eq!(note.units["enabled"], json!(true));
    // 默认插件：脚本不比较，设置和开关照常
    let def = &s.items[&id(ItemKind::Plugin, "reply-language")];
    assert!(!def.units.contains_key("script"));
    assert_eq!(def.units["settings"], json!({"lang": "zh"}));
    assert_eq!(
        s.items[&id(ItemKind::PluginOrder, "")].units["all"],
        json!(["note", "reply-language"])
    );
}

#[test]
fn plugin_changes_land_on_the_files_and_the_entry() {
    let codec = Fake::new();
    let p = with_plugins(&[
        ("note", "// data: {\"v\":1}\nmy code\n", true),
        (
            "reply-language",
            "// data: {\"lang\":\"zh\"}\nshipped code v1\n",
            false,
        ),
    ]);
    // 只改设置：本机的文件上改，照样批准着
    let mut t = snap(&p);
    t.items
        .get_mut(&id(ItemKind::Plugin, "note"))
        .unwrap()
        .units
        .insert("settings".into(), json!({"v": 2}));
    let out = materialize_with(&p, &t, &codec).unwrap();
    let f = &out.plugins["note"];
    assert_eq!(f.script, b"// data: {\"v\":2}\nmy code\n");
    assert_eq!(f.approved.as_deref(), Some(&f.script[..]));
    let cfg = tw_config::try_parse(&out.config_yaml).unwrap();
    assert_eq!(cfg.plugins[0].sha256, items::sha256_hex(&f.script));
    // 默认插件在本机没有：用这一份 Core 自带的，设置照目标
    let bare = with_plugins(&[]);
    let out = materialize_with(&bare, &snap(&p), &codec).unwrap();
    assert_eq!(
        out.plugins["reply-language"].script,
        b"// data: {\"lang\":\"zh\"}\nshipped code v1\n"
    );
    assert_eq!(out.plugins["note"].script, b"// data: {\"v\":1}\nmy code\n");
    // 删掉一个
    let mut t = snap(&p);
    t.items.remove(&id(ItemKind::Plugin, "note"));
    t.items.insert(
        id(ItemKind::PluginOrder, ""),
        Item {
            units: BTreeMap::from([("all".into(), json!(["reply-language"]))]),
            ..Default::default()
        },
    );
    let out = materialize_with(&p, &t, &codec).unwrap();
    assert!(!out.plugins.contains_key("note"));
    assert!(!out.config_yaml.contains("id: note"), "{}", out.config_yaml);
}

/// 改得了工具调用的插件：开着、而代码在本机没批准过时，落下去也不批准
#[test]
fn a_steering_plugin_from_elsewhere_waits_for_approval_here() {
    let codec = Fake::new();
    let src = "// data: {}\nsteer the tool calls\n";
    let theirs = with_plugins(&[("steerer", src, true)]);
    let mut t = snap(&theirs);
    // 带着原文件来（导入、同步都带着）
    t.items
        .get_mut(&id(ItemKind::Plugin, "steerer"))
        .unwrap()
        .files = theirs.plugins.get("steerer").cloned();
    let out = materialize_with(&with_plugins(&[]), &t, &codec).unwrap();
    let cfg = tw_config::try_parse(&out.config_yaml).unwrap();
    let e = &cfg.plugins[0];
    assert!(e.enabled);
    assert_eq!(out.plugins["steerer"].script, src.as_bytes());
    assert_ne!(
        e.sha256,
        items::sha256_hex(src.as_bytes()),
        "批准了别处来的代码"
    );
    assert_eq!(out.plugins["steerer"].approved, None);
    // 停用着来的照常批准：打开时才问
    let mut off = t.clone();
    off.items
        .get_mut(&id(ItemKind::Plugin, "steerer"))
        .unwrap()
        .units
        .insert("enabled".into(), json!(false));
    let out = materialize_with(&with_plugins(&[]), &off, &codec).unwrap();
    let cfg = tw_config::try_parse(&out.config_yaml).unwrap();
    assert_eq!(cfg.plugins[0].sha256, items::sha256_hex(src.as_bytes()));
    // 本机停用着、批准过同一份代码，别处把它打开了：这里照样要点头
    let off_here = with_plugins(&[("steerer", src, false)]);
    let mut t = snap(&off_here);
    t.items
        .get_mut(&id(ItemKind::Plugin, "steerer"))
        .unwrap()
        .units
        .insert("enabled".into(), json!(true));
    let out = materialize_with(&off_here, &t, &codec).unwrap();
    let cfg = tw_config::try_parse(&out.config_yaml).unwrap();
    assert!(cfg.plugins[0].enabled);
    assert_ne!(
        cfg.plugins[0].sha256,
        items::sha256_hex(&out.plugins["steerer"].script)
    );
    // 读回来和目标一样：不会因为待批准就当成本机又改了一次
    assert_eq!(
        snap(&out).items[&id(ItemKind::Plugin, "steerer")],
        t.items[&id(ItemKind::Plugin, "steerer")]
    );
    // 本机开着、批准过同一份代码：只改设置不用再批准
    let mine = with_plugins(&[("steerer", src, true)]);
    let mut t = snap(&mine);
    t.items
        .get_mut(&id(ItemKind::Plugin, "steerer"))
        .unwrap()
        .units
        .insert("settings".into(), json!({"x": 1}));
    let out = materialize_with(&mine, &t, &codec).unwrap();
    let cfg = tw_config::try_parse(&out.config_yaml).unwrap();
    assert_eq!(
        cfg.plugins[0].sha256,
        items::sha256_hex(&out.plugins["steerer"].script)
    );
}

/// 本机文件改了还没批准：比较按批准过的那一份，没批准的改动不往外带
#[test]
fn an_unapproved_edit_is_neither_exported_nor_lost() {
    let approved = "// data: {\"v\":1}\napproved code\n";
    let mut p = with_plugins(&[("note", approved, true)]);
    p.plugins.get_mut("note").unwrap().script =
        b"// data: {\"v\":1}\nedited, not approved\n".to_vec();
    let s = snap(&p);
    assert_eq!(
        s.items[&id(ItemKind::Plugin, "note")].units["script"],
        json!("// data: null\napproved code\n")
    );
    // 别处改了设置：本机那处没批准的改动留着，底稿和哈希换成新的
    let mut t = s.clone();
    t.items
        .get_mut(&id(ItemKind::Plugin, "note"))
        .unwrap()
        .units
        .insert("settings".into(), json!({"v": 2}));
    let out = materialize_with(&p, &t, &Fake::new()).unwrap();
    let f = &out.plugins["note"];
    assert_eq!(f.script, b"// data: {\"v\":1}\nedited, not approved\n");
    assert_eq!(
        f.approved.as_deref(),
        Some(&b"// data: {\"v\":2}\napproved code\n"[..])
    );
}

#[test]
fn client_preferences_merge_field_by_field() {
    let mut p = pack(&rich());
    p.client_prefs = Some(json!({"language": "zh", "menubar": {"show": "tokens", "rows": 2}}));
    let s = snap(&p);
    let prefs = &s.items[&id(ItemKind::ClientPrefs, "")];
    assert_eq!(
        prefs.units.keys().collect::<Vec<_>>(),
        ["language", "menubar.rows", "menubar.show"]
    );
    let mut t = s.clone();
    t.items
        .get_mut(&id(ItemKind::ClientPrefs, ""))
        .unwrap()
        .units
        .insert("menubar.show".into(), json!("speed"));
    let out = mat(&p, &t);
    assert_eq!(
        out.client_prefs,
        Some(json!({"language": "zh", "menubar": {"show": "speed", "rows": 2}}))
    );
}

// ─────────────────────────────────────────────────────────── 改名

#[test]
fn renaming_follows_every_reference() {
    let p = pack(&rich());
    type Check = fn(&Snapshot);
    let cases: Vec<(ItemKind, &str, &str, Check)> = vec![
        (ItemKind::Provider, "relay", "relay (2)", |s| {
            let g = &s.items[&ItemId::new(ItemKind::Group, "pool")].units["all"];
            assert_eq!(g["providers"], json!(["官方", "relay (2)"]));
            let r = &s.items[&ItemId::new(ItemKind::Route, "长上下文")].units["all"];
            assert_eq!(r["rules"][0]["to"], json!("relay (2)"));
            let o = &s.items[&ItemId::new(ItemKind::ProviderOrder, "")].units["all"];
            assert_eq!(o, &json!(["官方", "relay (2)", "chatgpt"]));
        }),
        (ItemKind::Proxy, "hk", "hk2", |s| {
            let c = &s.items[&ItemId::new(ItemKind::Provider, "relay")].units["conn"];
            assert_eq!(c["proxy"], json!("hk2"));
        }),
        (ItemKind::PriceSheet, "中转", "中转 (2)", |s| {
            let b = &s.items[&ItemId::new(ItemKind::Provider, "relay")].units["billing"];
            assert_eq!(b["pricing"], json!("中转 (2)"));
        }),
        (ItemKind::Key, "codex", "codex-2", |s| {
            let r = &s.items[&ItemId::new(ItemKind::Route, "长上下文")].units["all"];
            assert_eq!(r["rules"][0]["when"]["client"], json!("codex-2"));
        }),
        (ItemKind::Key, "default", "main", |s| {
            // 默认密钥原来是「叫 default 的那把」：改名之后要写明
            let g = &s.items[&ItemId::new(ItemKind::Global, "default_key")];
            assert_eq!(g.units["default_key"], json!("main"));
        }),
        (ItemKind::Alias, "fast", "quick", |s| {
            let k = &s.items[&ItemId::new(ItemKind::Key, "codex")].units["all"];
            assert_eq!(k["allow"], json!(["quick"]));
        }),
        (ItemKind::Group, "pool", "pool2", |s| {
            let r = &s.items[&ItemId::new(ItemKind::Route, "default")].units["all"];
            assert_eq!(r["rules"][0]["to"], json!("pool2"));
        }),
        (ItemKind::Route, "长上下文", "long", |s| {
            let k = &s.items[&ItemId::new(ItemKind::Key, "codex")].units["all"];
            assert_eq!(k["route"], json!("long"));
            let o = &s.items[&ItemId::new(ItemKind::RouteOrder, "")].units["all"];
            assert_eq!(o, &json!(["default", "long"]));
        }),
        (ItemKind::Route, "default", "main", |s| {
            let g = &s.items[&ItemId::new(ItemKind::Global, "default_route")];
            assert_eq!(g.units["default_route"], json!("main"));
        }),
        (ItemKind::SecurityRule, "content/脏话", "粗话", |s| {
            assert!(
                s.items
                    .contains_key(&ItemId::new(ItemKind::SecurityRule, "content/粗话"))
            );
        }),
    ];
    for (kind, old, new, check) in cases {
        let mut s = snap(&p);
        rename(&mut s, &id(kind, old), new);
        assert!(!s.items.contains_key(&id(kind, old)), "{old}");
        check(&s);
        // 改了名的那一份落得回去、过得了校验
        let out = materialize_with(&p, &s, &Fake::new()).unwrap_or_else(|e| panic!("{old}: {e}"));
        tw_config::try_parse(&out.config_yaml)
            .unwrap_or_else(|e| panic!("{old} → {new}: {e}\n{}", out.config_yaml));
        assert_eq!(normalized(&snap(&out)), normalized(&s), "{old} → {new}");
    }
    // 名字被占了、没有名字的项目：不动
    let mut s = snap(&p);
    let before = s.clone();
    rename(&mut s, &id(ItemKind::Provider, "relay"), "官方");
    rename(&mut s, &id(ItemKind::Global, "failover"), "x");
    assert_eq!(s, before);
    assert_eq!(free_name(&s, ItemKind::Provider, "relay"), "relay (2)");
    assert_eq!(free_name(&s, ItemKind::Provider, "new"), "new");
}

#[test]
fn retain_trims_the_orders_too() {
    let mut s = snap(&pack(&rich()));
    s.retain(&[id(ItemKind::Provider, "relay")].into_iter().collect());
    assert_eq!(
        s.items.keys().map(|i| i.to_string()).collect::<Vec<_>>(),
        ["provider `relay`", "provider_order"]
    );
    assert_eq!(
        s.items[&id(ItemKind::ProviderOrder, "")].units["all"],
        json!(["relay"])
    );
}
